# Review findings & recorded deviations

Deferred findings from rotating-model review panels. These are accepted
deviations or future work items — not blockers.

## M1: opencode-schema

- none outstanding (two wire bugs found by review were fixed in `fixup:` commits).

## M2: opencode-llm

- MINOR (executor): TS threads `Headers.CurrentRedactedNames` through the
  executor; Rust redacts only the fixed SENSITIVE_NAME set. No call sites need
  it today — revisit when custom secret-bearing headers are configurable.
- MINOR (executor): body truncation measures bytes with char-boundary
  fallback vs TS UTF-16 code units; truncation points differ for multibyte
  bodies. Test-only impact.
- MINOR (executor): `redact_url`/URL parsing is more lenient than WHATWG
  `URL.canParse`; non-sensitive URLs are passed through un-normalized.
- MINOR (executor): `secret_values` short-value filter uses chars() count vs
  UTF-16 length — astral-plane secrets only.
- MINOR (anthropic): `usage` payloads typed as raw `Value` vs TS
  schema-validated `AnthropicUsage`; shape validation happens at use sites.
- CONVENTION: f64-typed wire numbers serialize as x.0 vs integer — golden
  comparisons must be numeric-tolerant (applies to all future milestones).

## M3: opencode-core foundation

- MINOR (bus): TS `readAggregate` (paged, manifest-filtered read),
  `beforeAggregateRead` hook, `allBounded`/`SubscriberOverflowError` not
  ported — deliberate deferral until a consumer needs them (M5 session engine).
- MINOR (storage): migration journal seeds all rows with one now_ms() value;
  TS stamps per row. Not observable in fresh-DB flows.
- RefreshListener bridge to models.dev catalog implemented (M3.6).

## Milestone 4 (tools) review panel

Panel: glm-5.3-thinking (agentic, partial pass) + deepseek-v4.1-flash-thinking (diff review).

### Accepted & fixed
- **webfetch timeout ignored (BLOCKER)**: `_timeout_ms` was computed then discarded; now threads a `Duration` through the `HttpClient` seam into reqwest's per-request timeout.
- **websearch MCP args sent `null` instead of omitted (BLOCKER)**: `contextMaxCharacters: null` / `model_name: null` reached the wire; TS `JSON.stringify` drops undefined. Now omitted entirely.
- **bash `expand()` auto-var regex (MINOR)**: `(?i)\$(HOME|PWD|PSHOME)([\\/])?` matched `$HOME` inside `$HOMEWORK`; TS uses a lookahead `(?=$|[\\/])`. Replaced with a non-lookahead equivalent `(?:([\\/])|$)` + regression test.
- **bash `preview()` byte-vs-UTF-16 slicing (MINOR)**: TS `slice(-30000)` counts UTF-16 units; now an `encode_utf16` walk.
- **webfetch `<pre><code>` double markup (MINOR)**: inline code backticks are now suppressed inside `<pre>` fences.
- **webfetch `attr_value` panic risk (MINOR)**: Unicode lowercase can shift byte offsets; uses ASCII lowercasing now.
- **Stray repo-root files**: `bom.rs diff.rs edit.rs write.rs mod.rs err.txt out.txt` (implementer leftovers) — deleted.

### Rejected (pinned TS behaves identically)
- **apply_patch delete `deletions` count**: TS `split("\n").length` includes the trailing empty segment — port matches.
- **patch pure-addition insertion index**: TS inserts before the trailing empty line — port matches.
- **webfetch content-type case sensitivity**: TS `contentType.includes("text/html")` is case-sensitive too — port matches.
- **SSE `data: ` with-space parsing**: TS `line.startsWith("data: ")` requires the space — port matches.

## Milestone 5 (session engine) review panel

Panel: glm-5.3-thinking (agentic, self-directed review with subagents) + deepseek-v4.1-flash-thinking (chunked diff + focused file-pair passes).

### Accepted & fixed
- **Reasoning `providerMetadata` dropped on delta/end (MAJOR)**: processor now merges `provider_metadata` into the reasoning part on both paths (processor.ts:299, 308-310). Anthropic thinking signatures survive for replay.
- **Cancelled `ask` leaks ghost entries (MAJOR)**: `PermissionService::ask`/`QuestionService::ask` now remove the pending entry via a drop guard (`Effect.ensuring` parity — permission/index.ts:101-108).
- **`remove()` semantics (MAJOR)**: matches TS — initial `get` NotFound propagates; errors after it soft-fail to a log (session.ts:606-627); `EventBus::remove(session_id)` now drops the session's subscriptions.
- **Sequential-after-stream tool dispatch (MAJOR)**: `dispatch_tool_calls` now forks each `ToolCall` immediately (`FiberSet.run` parity — native-runtime.ts:103-140); tools run concurrently with the stream and each other; settlements drain after.
- **Subtask failure prefix (MINOR)**: `Tool execution failed: ` prefix restored (prompt.ts:424); `failed_part` converts pending parts (prompt.ts:413-428) with start-fallback and metadata-drop.
- **Compaction `overflow` always serialized (MINOR)**: `CompactionCreate.overflow` is now `Option<bool>`; the auto-overflow path passes `None` (prompt.ts:1166 leaves it absent).
- **`set_workspace(None)` couldn't clear (MINOR)**: `SessionPatch.workspace_id` is now tri-state `SetClear<String>` (session.ts:814-821 spread semantics).
- **Wildcard matcher not dot-all (MINOR)**: `(?s)` added (util/wildcard.ts:17-18).
- **Lock held across publishes (MINOR)**: `PermissionService::reply` restructured to collect publishes/sends under the lock but run them after release; `QuestionService::reply`/`reject` drop before publishing.
- **Negative retry-after clamped (MINOR)**: `next` computed in f64 (retry.ts:196-198 keeps past timestamps).
- **`clone_part` ID swallow (MINOR)**: propagates like the message path.
- **Snapshot `patch` soft-fail (MINOR)**: unknown snapshot id returns `{hash, files: []}` (snapshot/index.ts:349-361); `DisabledSnapshot::patch` likewise.
- **Metadata-sink defaults (MINOR)**: title/metadata written through unmodified (tools.ts:67-80) — absent keys stay absent.
- **`AgentRegistryInput::default()` whitelisted `/*` (MAJOR, deepseek)**: empty `data_dir`/`tmp_dir` no longer produce `/*` external_directory allow rules.

### Rejected (pinned TS behaves identically / impractical)
- **`localeCompare` sorting (MINOR)**: references/skills sort byte-order vs TS `localeCompare` — exact parity needs ICU collation; divergence only for non-ASCII names in prompt content, not wire shape.
- **HTTP-date parsing subset of `Date.parse` (MINOR)**: offset-less ISO strings fall back to exponential backoff; TS behavior is timezone-dependent (local time), so exact parity is under-specified. Recorded divergence.
- **check_message_error content-filter BLOCKER**: withdrawn by the reviewer itself — the Rust guard mirrors `finished && !error` (prompt.ts:1295-1296).

## Milestone 6 (server) review panel

Panel: glm-3.3-thinking (agentic) + deepseek-v4.1-flash-thinking (agentic pass, budget-limited).

### Accepted & fixed
- **Compression middleware hangs v2 SSE streams (BLOCKER)**: the port buffered every body up to 16MB — SSE responses never completed. Fixed with the TS `body._tag !== "Uint8Array"` passthrough equivalent: streaming bodies (no exact size hint) are never collected (compression.ts:39-40).
- **Payload-400 vs session-404 precedence (MAJOR)**: six v1 handlers (prompt, prompt-async, command, shell, init, revert) checked the session before parsing the payload; TS decodes payloads in middleware first. Reordered; branded-id checks moved up too (schema-error decode-time semantics).
- **v2 ID-prefix checks used `msg_`/`ses_` (MAJOR, deepseek)**: TS `Schema.isStartsWith` checks the bare prefix (`msg`, `session/schema.ts:10,19`). Fixed to bare prefixes.
- **`truncate_reason` counts chars, not UTF-16 (MINOR)**: now `encode_utf16` like TS `reason.length` (schema-error.ts:10-13).
- **PTY-connect bypass accepted empty pty id (MINOR)**: `[^/]+` requires non-empty (pty-ticket.ts:9-11).
- **v1 disposed-terminator matched Injected frames (MINOR)**: TS only inspects the standard GlobalBus event shape (handlers/event.ts:42-62).

### Recorded divergences (not fixed)
- **Env-flag snapshots at construction** (FenceLayer, LocationLayer, AuthConfig): TS reads per-request; CLI usage unaffected — recorded as known divergence.
- **InstanceStore holds the entries lock across factory config I/O** — scalability TODO added in state.rs, no wire divergence.

## Milestone 7 (deferred subsystems) review panel

Panel: glm-5.3-thinking (agentic, 50% budget) + deepseek-v4.1-flash-thinking (agentic, complete).

### Accepted & fixed
- **`getSmallModel` omits the azure short-circuit (MAJOR, deepseek)**: azure/azure-cognitive-services now return None before model selection (provider.ts:1963-1966).
- **VCS `file_from_git_header` char-index-as-byte-index (MAJOR, deepseek)**: `parse_quoted_path` now returns byte offsets — panicked on multibyte quoted paths (vcs.rs vs project/vcs.ts:33-76).
- **Worktree remove/reset leaked `WorktreeListFailedError` (MAJOR, deepseek)**: the list call wraps under its caller's tag — `RemoveFailedError`/`ResetFailedError` (worktree/index.ts:400-402, 541-543). Plus the reset message colon (:602).
- **GitSnapshot blocking git subprocesses on the async runtime (MAJOR, agentic)**: all trait methods now run their sync bodies via `spawn_blocking` (TS Effect fibers).
- **LSP `end_position` counted Unicode scalars, not UTF-16 units (MINOR, deepseek)** (client.ts:87).
- **`RepositoryCache::keyed_lock` leaked an Arc per call (MINOR, deepseek)**: leaks once per unique path, reused from the map.
- **engine.rs `.expect("ses id")` production unwraps (MINOR, agentic)**: now fall through to the `Slug.create()` fallback.

### Recorded divergences (not fixed)
- **MCP stdio `recv_until` is single-consumer** (responses mutex held across recv; non-matching responses consumed). Latent only — single `call_tool` call site; TS SDK correlates per pending-id. Noted in transport.rs.
- **MCP/LSP stdin writes** happen with concurrent reader tasks on stdout (deadlock neutralized); adversarial servers that reply before reading stdin are out of scope.
- The agentic reviewer's ~50%-budget handoff listed `background.rs` cancel/spawn race and EngineStore `Arc::as_ptr` eviction as *candidates* to verify — neither verified against TS, not fixed; re-check in the M8 pre-TUI audit.

## Milestone 8 (TUI) review panel

Panel: glm-5.3-thinking (agentic, complete through focus areas 1-4) + deepseek-v4.1-flash-thinking (failed budget twice mid-exploration; both runs offloaded to the agentic reviewer's verified findings). Focus area 5 (transcript rendering) reviewed only by the agentic reviewer's handoff — see below.

### Accepted & fixed
- **Prompt `submit()` missing the `props.disabled` gate (MAJOR)**: TS gates on `permissions().length > 0 || questions().length > 0` (`routes/session/index.tsx:241`, `prompt/index.tsx:957`); Rust now rejects submit while permission or question prompts are stored (state/prompt.rs).
- **`clear_prompt` counted Unicode scalars, not UTF-16 units (MINOR)**: TS `.length` (`prompt/index.tsx:1273`); now `encode_utf16().count()` (state/prompt.rs).
- **`workspace_list` dead entry in `APP_KEYBINDS` (MINOR)**: not present anywhere in the TS TUI source; removed (keymap/mod.rs).

### Recorded divergences (not fixed)
- **Hydrate "live-tracking wins" under the lock architecture**: TS hydrate merges events arriving during its awaits via the tracker; the Rust runtime holds the app lock across the hydrate awaits, so concurrent updates are structurally impossible — events queue and apply after the merge (live wins by ordering). Tracker kept for the M6.7-equivalent hydrate semantics and tested directly (sync.rs:2066+).
- **`fatal` bootstrap contract**: `.context("fatal")` is a stringly type-level nit — the caller branches on the same `fatal` bool, so behavior is correct. Recorded, not fixed.
- **`input_paste` `preventDefault: false`** (`keybind.ts:162`): a browser-only flag (native paste handler also runs); ratatui handles paste via `Msg::Paste`. Non-goal.
- **`Moved.timestamp`**: `EpochMillis = i64` (opencode-schema), so the `.max(0)` clamp is a dead guard, not a flooring bug. Recorded.

### Withdrawn/rejected by verification
- "Suspend rewrite skips user `input_undo` overrides" — TS itself guards `keybinds.input_undo === undefined` (`config/index.tsx:105`); the Rust port is faithful.
- SSE reconnect semantics F8 and VcsBranchUpdated F9 — verified correct / not a divergence.

## CLI milestone review panel

Panel: glm-5.3-thinking (agentic — two runs, both died mid-flight at turns 50/35 with substantial verification work completed; client.rs verified "matches the SDK v2 semantics", run/tui/attach/providers/models comparisons in progress) + deepseek-v4.1-flash-thinking (three runs — two budget deaths, one external_directory denial; no findings emitted). Orchestrator spot-verification completed the highest-priority items directly.

### Accepted & fixed
- **Catalog capability-field schema drift (MAJOR, C6/C9 finding)**: live models.dev data fails `temperature` parsing (the pinned TS schema reads plain booleans). `Catalog::Model` capability fields (`attachment`, `reasoning`, `temperature`, `tool_call`) now tolerate object forms (presence ⇒ supported) and default to `true` when absent (crates/opencode-core/src/catalog/types.rs).
- **`cleanup_loop_fires_after_the_first_delay` flake (test infra)**: the +10ms cleanup tick can race `track()` creating the gitdir; a wasted tick waits the full interval. Test now uses a short retry interval (crates/opencode-core/src/session/snapshot.rs).

### Verified equivalent by the panel/orchestrator
- `client.rs` matches the TS SDK v2 rewrite semantics for the v1 API.
- The `run` finish path (prompt error → exit 1; accumulated session.error → exit 1) is equivalent. **Recorded nuance**: TS `finish()` returns early in attach mode without awaiting loop completion; the Rust attach loop consumes events until idle — an attach-mode session.error that TS would not observe can set exit 1 in Rust.
- tui.ts/attach.ts flag matrices, validation order, PWD-relative project resolution verified against the TS during C8 implementation tests.

### Panel failures recorded
- The agentic reviewer's budget (~50 turns) is insufficient for a full CLI review; split-scope re-runs also died. deepseek failed on budget ×2 and external_directory ×1. Priority-5 spot-checks (mcp.rs, pr.rs, session.rs, db.rs, stats.rs vs TS) were therefore only partially covered — C9's implementation reports and the e2e suite stand as the primary coverage.

## Post-12.4 close-out (final acceptance)

### Deferred candidates — verified & fixed
- **`background.rs` cancel/spawn race (from the M7 panel handoff)**: CONFIRMED.
  TS forks runs into the job's Effect scope, so a cancel that closes the
  scope also interrupts a fork landing concurrently with it. The Rust port
  pushed the `AbortHandle` into `job.tasks` under a second lock acquisition
  after `tokio::spawn` — a cancel slipping between insert and push left the
  run executing forever. `start`/`extend` now re-check `status == Running`
  at push time and abort otherwise (fork-into-closed-scope semantics);
  regression test `cancel_interrupts_the_in_flight_run` (background.rs).
- **`EngineStore` `Arc::as_ptr` eviction (from the M7 panel handoff)**:
  CONFIRMED (leak, not correctness). Entries held a strong services `Arc`
  keyed by its address and were never removed — every dead instance leaked
  its engine for the process lifetime. Entries now hold a `Weak`, and
  lookups lazily evict lapsed entries; a live upgrade also proves the
  address key still belongs to that instance (address reuse can never
  produce a stale match). TS gets the same lifetime from per-instance
  Effect scopes.

### TUI acceptance clarification (M9 row, "headless vt100 snapshot tests")
- Satisfied by ratatui `TestBackend` buffer assertions rather than a vt100
  emulator: the TUI e2e renders the whole app through `ui::view` into a
  100x30 buffer over the real M6 server wire and asserts cell content
  (`e2e.rs`), and footer/transcript widgets have dedicated render unit
  tests. vt100-level golden snapshots would primarily test ratatui's escape
  sequences, not app rendering; recorded as a deliberate deviation.

### Plan-doc numbering errata
- The milestone table's row numbering diverged from the executed series:
  the TUI shipped as 8.1–8.8, the CLI as C1–C9, the mock-LLM E2E as
  10.1–10.4, the parity harness as 12.1–12.3, and the ACP adapter as 12.4
  (completing plan row 7). Acceptance rows map to: row 8 → C-series, row 9
  → 8.x, row 12 → 12.x.

### Live E2E tier (final acceptance re-run)
- All 13 live scenarios pass individually on current HEAD. Full-suite runs
  (3 × ~20 min) each surfaced a rotating pair of failures — different
  scenarios each time, every one green on isolated re-run (b2_glm needed a
  sharper relative-path directive; b4_qwen a mandatory "call the task tool"
  phrasing). Recorded as inherent quality-tier nondeterminism: the live tier
  is a best-effort smoke, not a CI gate; the deterministic contract lives in
  the mock-LLM e2e + parity suites.

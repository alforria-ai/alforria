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

## Final full-port review panel (6 agents, post-12.4 close-out)

Six parallel review agents compared the Rust port against the pinned TS
reference (88c6c7a): llm, session engine, tools, server, CLI+ACP, TUI+foundation.

### Fixed in this pass
- **CRITICAL — leftover debug `fs::write().unwrap()` in the halt branch of the
  LLM stream loop** (llm/src/route/client.rs): panicked production machines
  without /tmp/opencode; removed.
- **CRITICAL — ACP agent sequential dispatch deadlock** (opencode/src/acp/mod.rs):
  `run()` awaited each request's dispatch inline, so inbound responses to
  outbound requests (`session/request_permission`, `fs/write_text_file`) were
  never routed during a `session/prompt` — the prompt deadlocked on the first
  permission ask — and `session/cancel` could not interrupt a running prompt.
  Requests (and cancel notifications) now dispatch on spawned tasks, matching
  the SDK's fire-and-forget `processMessage`.
- **CRITICAL — ACP `default_model_from_config` picked the lowest-priority
  model** (opencode/src/acp/directory.rs): the priority dimension of
  `Provider.sort` was inverted; the default was `sort[N]` instead of
  `sort[0]`. Regression test added.
- **MAJOR — ACP effort validation accepted any string** (opencode/src/acp/mod.rs):
  `variant == DEFAULT_VARIANT_VALUE` iterated keys instead of comparing the
  value; "default"-variant models validated everything. Now follows
  `hasVariant` (service.ts:930-933).

### Open findings — llm (opencode-llm)
- **MAJOR**: openai-chat finalizes tool calls eagerly (isParsableJson) which
  downgrades terminal finish reason `tool-calls` → `stop` and streams
  tool-call events mid-response (openai_chat.rs:722-737 vs
  openai-chat.ts:429-470). Test at :1214 codifies the divergence.
- **MAJOR**: openai-responses route drops the static `store: false` default
  → reasoning replay emits `item_reference` where TS replays encrypted
  reasoning (openai_responses.rs:1248-1262, 256-263 vs openai-responses.ts:984-992).
- MINOR: `onOutputItemDone` accepts empty `call_id`/`name`
  (openai_responses.rs:1003-1010). MINOR: bedrock stores/echoes empty-string
  signature (bedrock_converse.rs:923-973). MINOR: providerMessage trailing
  ": " for empty error bodies (route/executor.rs:688-695). MINOR: anthropic
  stream frames silently tolerated without `type`
  (anthropic_messages.rs:371-381). MINOR: providerMetadata passthrough keeps
  undeclared usage fields (openai_chat.rs:621-647). MINOR: alphabetized JSON
  key order (no preserve_order). MINOR: gemini accepts `functionCall` without
  `args` (gemini.rs:271-276). MINOR: openai-chat strict tool-call index
  validation TS doesn't perform (openai_chat.rs:651-660).

### Open findings — session engine (opencode-core)
- **MAJOR**: question rejection does not block the agent loop: `rejected()`
  maps to a generic failed-tool message; TS `Question.RejectedError` +
  `instanceof` sets `ctx.blocked` (session/question.rs:258,
  session/tools.rs:181-189, processor.rs:572-582 vs question/index.ts:27,
  processor.ts:200-201).
- MINOR: cleanup awaits tool settlements serially (N×250ms) instead of
  concurrently (processor.rs:1892-1899 vs processor.ts:585-588). MINOR:
  aborted tool completion drops attachments (session/tools.rs:208-220 vs
  tools.ts:116-127). MINOR: stream-error path leaves tool tasks detached,
  can write after cleanup (session/llm.rs:688-702). MINOR: `title_from_text`
  counts scalars not UTF-16 (session/loop.rs:1129-1142 vs prompt.ts:247-249).

### Open findings — tools (opencode-core)
- **MAJOR**: task input schema marks `command` required; TS keeps it optional
  (tool/task.rs:218-225 vs tool/task.ts:47-56).
- **MAJOR**: background subagents accepted but not implemented at runtime —
  no `BACKGROUND_STARTED`/`BACKGROUND_UPDATED`, no jobId metadata, no
  promotion (tool/task.rs:276-390 vs tool/task.ts:92-359).
- **MAJOR**: task foreground runs are not cancellable; `ops.cancel` never
  wired to `ctx.abort` (tool/task.rs:276-390 vs tool/task.ts:320-358).
- **MAJOR**: `experimental.primary_tools` denies dropped from subagent
  permission (`_primary_tools` unused) (tool/task.rs:281 vs tool/task.ts:143-155).
- **MAJOR**: webfetch treats non-2xx as success — no `filterStatusOk`
  equivalent (tool/webfetch.rs:211-217 vs webfetch.ts:84-97).
- **MAJOR**: webfetch turns `image/svg+xml` into an attachment; TS serves it
  as text; also excludes bmp/tiff (tool/webfetch.rs:22-29 vs util/media.ts).
- **MAJOR**: websearch MCP call has no timeout; production client has no
  default timeout (tool/mcp_websearch.rs:90-113, server engine.rs:1077-1105
  vs mcp-websearch.ts:74-102 — TS: 25s).
- **MAJOR**: bash run loop can drop output between process exit and pipe
  drain (tool/shell/mod.rs:790-834 vs shell.ts:486-595).
- MINOR: webfetch missing Cloudflare-challenge retry (webfetch.ts:99-112).
  MINOR: `execute` is a projection-port; code-mode interpreter unported by
  design (tool/code_mode.rs). MINOR: PowerShell parsing is a
  whitespace-split fallback vs tree-sitter grammar (tool/shell/parse.rs:156-195).
  MINOR: apply_patch/lsp relative-path helpers fall back to absolute path
  instead of `../` (tool/apply_patch.rs:77-83, tool/lsp.rs:185-188).
  MINOR: locale-vs-byte ordering in read/registry sorts (read.rs:444,
  registry.rs:150). MINOR: grep searches binary files (ripgrep.rs:84-109).
  MINOR: edit levenshtein counts scalars not UTF-16 (edit.rs:182-203).

### Open findings — server (opencode-server)
- MINOR: missing `installation.updated` GlobalBus emission after upgrade
  (routes/v1/global_control.rs:189-216 vs handlers/global.ts:107-114).
  MINOR: PTY create drops the `shell.env` plugin trigger (no plugin runtime)
  (pty/routes.rs:228-245, 321-336 vs handlers/pty.ts:69-82). MINOR (latent):
  UI catch-all doesn't strip the leading slash and never sets CSP
  (routes/ui.rs:34-46 vs shared/ui.ts:55-76).

### Open findings — ACP + CLI (opencode)
- **MAJOR**: event subscription never transitions to "disconnected"; idle
  waiters never rejected on SSE loss (acp/event.rs:110-161 vs
  acp/event.ts:144-182).
- **MAJOR**: `percent_decode` corrupts non-ASCII paths byte-as-char
  (acp/content.rs:329-346 vs decodeURIComponent semantics).
- **MAJOR**: `--mini` validated but silently runs the single-prompt path
  (cmd/run.rs:596-792 vs cli/cmd/run.ts:833-905).
- MINOR: `authenticate` returns null not {} (acp/mod.rs:159-167). MINOR:
  error message suffixes dropped (`session not found: X` etc.) (acp/mod.rs).
  MINOR: no zod-style param validation (-32602 vs -32603 confusion)
  (acp/mod.rs). MINOR: `location_from` dedups only adjacent repeats
  (acp/tool.rs:54-76). MINOR: `"data": null` always serialized on errors /
  metadata null vs dropped undefined (acp/jsonrpc.rs:73-81, acp/tool.rs:166-197).
  MINOR: attach-mode exit code from session errors diverges (cmd/run.rs:787-790
  vs run.ts:839-843). MINOR: model-option/command sorting lowercases
  (acp/config_option.rs:190-217, directory.rs:316-321). MINOR: data-URL
  regex rejects extra params (acp/tool.rs:251-263). MINOR:
  `available_commands_update` can overtake the response (acp/mod.rs:856-880).
  MINOR: no `requestPermission`-capability auto-reject (acp/permission.rs:176-244).
  MINOR: context-limit cache absent (acp/usage.rs:117-123). MINOR: `time.end:
  null` treated as finished (cmd/run_events.rs:337,354). MINOR: simplified
  applyPatch in permission bridging (acp/permission.rs:285-343).

### Open findings — TUI + foundation
- **MAJOR**: `ConfigV2Compat.lower` not ported: V2 keys silently ignored /
  array-form skills fatally fails / V2 permissions silently accepted where
  TS rejects (config/precedence.rs:560-583, config/schema.rs:1254 vs
  config/v2-compat.ts:83-132, config.ts:187-189).
- MINOR: `normalizeLoadedConfig` legacy-key strip (theme/keybinds/tui) not
  ported. MINOR: catalog custom-URL cache file uses FNV-1a not SHA-1
  (catalog/service.rs:421-437 vs models-dev.ts:161-164). MINOR: git runner
  drops TS global flags (`--no-optional-locks`, `core.quotepath=false`)
  (git.rs:159-174 vs git/index.ts:6-13). MINOR: transcript formatter drops
  tool input for completed/errored tools and ignores TS truthiness gates
  (tui/src/transcript.rs:163-178 vs transcript.ts:101-104). MINOR:
  account-config failures fatal (config/precedence.rs:347 vs TS catch+log).

### Panel verdicts
- **llm**: "highly faithful in lowering logic, usage math, and shared
  machinery" — eager finalization + store:false are the real gaps.
- **session engine**: "a meticulous port — one MAJOR user-visible divergence
  (question rejection) plus four minor edge-path divergences."
- **tools**: "edit/read/grep/glob and truncation/permission seams are
  effectively byte-parity; task/webfetch/websearch/bash-drain need fixing
  before parity."
- **server**: "a notably faithful, fixture-locked transcription — two missing
  event/plugin side effects and a latent UI divergence, none critical."
- **ACP+CLI**: "the ACP wire surface is largely faithful; sequential dispatch
  and the model-priority inversion needed fixing" (both fixed in this pass).
- **TUI+foundation**: "exceptionally faithful in TUI state machine, keymap,
  storage, and skill areas; the real gap is the un-ported config v2-compat
  lowering layer."

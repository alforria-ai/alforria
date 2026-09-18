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

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

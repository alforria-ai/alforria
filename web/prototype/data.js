// Synthetic fleet for the alforria web prototype. Every session, path, diff and
// cost below is invented demonstration material, not real usage data.

const MIN = 60_000;
export const NOW = Date.now();
const ago = (m) => NOW - m * MIN;

export const server = {
  address: "192.168.1.196:4096",
  version: "0.1.0",
  auth: "basic",
};

export const projects = [
  { id: "p1", name: "opencode-rs", path: "~/repos/opencode-rs", branch: "main" },
  { id: "p2", name: "libertai-cli", path: "~/repos/libertai-cli", branch: "feat/alforria-engine" },
  { id: "p3", name: "pyaleph-rs", path: "~/repos/pyaleph-rs", branch: "main" },
  { id: "p4", name: "libertai-website", path: "~/repos/libertai-website", branch: "pricing-2026" },
  { id: "p5", name: "search-service", path: "~/repos/search-service", branch: "main" },
];

// state: waiting | working | retry | fault | idle
// now: { kind, text } where kind is a tool name, "writing", "thinking", or a state word.
export const sessions = [
  {
    id: "s1", project: "p1", title: "Port v2 prompt runner", agent: "build", model: "qwen3-coder-480b",
    state: "working", now: { kind: "bash", text: "cargo nextest run -p alforria-server --no-fail-fast" },
    todos: [4, 7], ctx: 63, cost: 1.84, since: ago(38), tokens: 81_400,
  },
  {
    id: "s1a", parent: "s1", project: "p1", title: "Find v2 session routes", agent: "explore", model: "qwen3-coder-30b",
    state: "idle", now: { kind: "done", text: "returned 14 routes" },
    todos: null, ctx: 22, cost: 0.06, since: ago(31), tokens: 28_900,
  },
  {
    id: "s1b", parent: "s1", project: "p1", title: "Audit session_message projection", agent: "explore", model: "qwen3-coder-30b",
    state: "working", now: { kind: "grep", text: "session_message  crates/alforria-core" },
    todos: null, ctx: 18, cost: 0.03, since: ago(2), tokens: 23_100,
  },
  {
    id: "s2", project: "p1", title: "Fix SSE heartbeat reconnect", agent: "build", model: "glm-4.6",
    state: "waiting", now: { kind: "permission", text: "bash  git push origin fix/sse-heartbeat" },
    todos: [5, 5], ctx: 41, cost: 0.52, since: ago(6), tokens: 52_800, branch: "fix/sse-heartbeat",
  },
  {
    id: "s3", project: "p1", title: "Web client store scaffold", agent: "plan", model: "kimi-k2",
    state: "waiting", now: { kind: "question", text: "How should the store normalize streamed parts?" },
    todos: [1, 4], ctx: 27, cost: 0.21, since: ago(1), tokens: 34_600,
  },
  {
    id: "s4", project: "p1", title: "Snapshot revert on renamed files", agent: "build", model: "qwen3-coder-480b",
    state: "idle", now: { kind: "done", text: "finished · 3 files changed" },
    todos: [6, 6], ctx: 55, cost: 0.97, since: ago(124), tokens: 70_400,
  },
  {
    id: "s5", project: "p2", title: "Embed alforria engine", agent: "build", model: "qwen3-coder-480b",
    state: "working", now: { kind: "edit", text: "src/engine/mod.rs" },
    todos: [2, 6], ctx: 48, cost: 1.31, since: ago(52), tokens: 61_400,
  },
  {
    id: "s5a", parent: "s5", project: "p2", title: "Audit feature flags", agent: "general", model: "glm-4.6",
    state: "working", now: { kind: "grep", text: "cfg(feature  src/" },
    todos: null, ctx: 12, cost: 0.02, since: ago(1), tokens: 15_300,
  },
  {
    id: "s6", project: "p2", title: "SSO token refresh", agent: "build", model: "glm-4.6",
    state: "waiting", now: { kind: "permission", text: "edit  src/auth/token.rs  +14 −5" },
    todos: [3, 4], ctx: 36, cost: 0.44, since: ago(3), tokens: 46_100,
  },
  {
    id: "s7", project: "p2", title: "Release notes 0.9", agent: "build", model: "glm-4.6",
    state: "idle", now: { kind: "done", text: "finished 41m ago" },
    todos: [3, 3], ctx: 19, cost: 0.12, since: ago(41), tokens: 24_300,
  },
  {
    id: "s8", project: "p3", title: "Message ingestion benchmark", agent: "build", model: "qwen3-coder-480b",
    state: "retry", now: { kind: "retry", text: "rate limited · attempt 2 in 8s" },
    todos: [2, 5], ctx: 44, cost: 0.66, since: ago(17), tokens: 56_300,
  },
  {
    id: "s9", project: "p3", title: "Port balances endpoint", agent: "build", model: "qwen3-coder-480b",
    state: "fault", now: { kind: "fault", text: "ContextOverflowError · 131k of 128k" },
    todos: [3, 6], ctx: 100, cost: 1.02, since: ago(9), tokens: 131_000,
  },
  {
    id: "s10", project: "p3", title: "Clippy sweep", agent: "build", model: "qwen3-coder-30b",
    state: "idle", now: { kind: "done", text: "finished · 22 files changed" },
    todos: [4, 4], ctx: 30, cost: 0.18, since: ago(203), tokens: 38_400,
  },
  {
    id: "s11", project: "p4", title: "Pricing table refresh", agent: "build", model: "kimi-k2",
    state: "waiting", now: { kind: "permission", text: "doom loop  read src/data/pricing.ts ×3" },
    todos: [2, 5], ctx: 52, cost: 0.39, since: ago(9), tokens: 66_500,
  },
  {
    id: "s12", project: "p4", title: "Lighthouse fixes", agent: "build", model: "glm-4.6",
    state: "working", now: { kind: "webfetch", text: "pagespeed.web.dev/analysis?url=libertai.io" },
    todos: [1, 4], ctx: 23, cost: 0.15, since: ago(14), tokens: 29_400,
  },
  {
    id: "s13", project: "p5", title: "Add SearXNG fallback", agent: "build", model: "qwen3-coder-480b",
    state: "working", now: { kind: "write", text: "src/providers/searxng.rs" },
    todos: [3, 5], ctx: 39, cost: 0.58, since: ago(27), tokens: 49_900,
  },
  {
    id: "s14", project: "p5", title: "Rate limiter tests", agent: "build", model: "qwen3-coder-30b",
    state: "idle", now: { kind: "done", text: "finished · 2 files changed" },
    todos: [2, 2], ctx: 14, cost: 0.07, since: ago(66), tokens: 17_900,
  },
];

// The interrupt queue: permissions (once / always / reject) and questions, oldest first.
export const queue = [
  {
    id: "q1", session: "s11", kind: "doom_loop", since: ago(9),
    title: "Repeated identical call",
    detail: "The agent called read with identical input 3 times in a row.",
    command: "read  src/data/pricing.ts",
    patterns: ["read src/data/pricing.ts"],
  },
  {
    id: "q2", session: "s2", kind: "bash", since: ago(6),
    title: "Run command",
    command: "git push origin fix/sse-heartbeat",
    patterns: ["git push *"],
  },
  {
    id: "q3", session: "s6", kind: "edit", since: ago(3),
    title: "Edit file",
    file: "src/auth/token.rs", add: 14, del: 5,
    patterns: ["src/auth/*"],
    diff: [
      ["hunk", "@@ -18,8 +18,17 @@ impl TokenStore {"],
      ["ctx", 18, "    pub async fn access_token(&self) -> Result<String> {"],
      ["del", 19, "        let token = self.load()?;"],
      ["del", 20, "        Ok(token.access)"],
      ["add", 19, "        let mut token = self.load()?;"],
      ["add", 20, "        if token.expires_at - Utc::now() < Duration::minutes(2) {"],
      ["add", 21, "            token = self.refresh(&token.refresh).await?;"],
      ["add", 22, "            self.save(&token)?;"],
      ["add", 23, "        }"],
      ["add", 24, "        Ok(token.access)"],
      ["ctx", 25, "    }"],
    ],
  },
  {
    id: "q4", session: "s3", kind: "question", since: ago(1),
    title: "State shape",
    question: "How should the client store normalize streamed message parts?",
    multiple: false,
    options: [
      { label: "Flat map keyed by part id", description: "Deltas apply in O(1); messages hold ordered part ids. Recommended." },
      { label: "Nested under each message", description: "Simpler reads; every delta needs a message lookup first." },
      { label: "Per-session event log", description: "Replayable views; roughly 3× the memory at fleet scale." },
    ],
  },
];

// Later arrivals, injected by the simulation to show the queue filling live.
export const arrivals = [
  {
    at: 35_000,
    item: {
      id: "q5", session: "s13", kind: "bash",
      title: "Run command",
      command: "cargo add reqwest --features json,rustls-tls",
      patterns: ["cargo add *"],
    },
    now: { kind: "permission", text: "bash  cargo add reqwest --features json,rustls-tls" },
  },
];

// Scripts the simulation walks through for working sessions.
export const activity = {
  s1: [
    ["bash", "cargo nextest run -p alforria-server --no-fail-fast"],
    ["read", "crates/alforria-server/src/routes/v2/session.rs"],
    ["edit", "crates/alforria-server/src/routes/v2/session.rs"],
    ["writing", "Wiring the v1 engine's prompt loop into the v2 handler…"],
    ["bash", "cargo nextest run -p alforria-server v2::"],
  ],
  s1b: [
    ["grep", "session_message  crates/alforria-core"],
    ["read", "crates/alforria-core/src/storage/projection.rs"],
    ["writing", "The projection is only written by the v2 runner…"],
  ],
  s5: [
    ["edit", "src/engine/mod.rs"],
    ["bash", "cargo check --features alforria"],
    ["read", "Cargo.toml"],
    ["writing", "Gating the embedded server behind the alforria feature…"],
    ["edit", "src/main.rs"],
  ],
  s5a: [
    ["grep", "cfg(feature  src/"],
    ["read", "src/engine/config.rs"],
    ["writing", "Found 9 feature gates; 2 are dead…"],
  ],
  s12: [
    ["webfetch", "pagespeed.web.dev/analysis?url=libertai.io"],
    ["thinking", "LCP is the hero image; it ships as an unsized PNG"],
    ["edit", "src/components/Hero.astro"],
    ["bash", "npm run build"],
  ],
  s13: [
    ["write", "src/providers/searxng.rs"],
    ["edit", "src/providers/mod.rs"],
    ["bash", "cargo test providers::searxng"],
    ["writing", "Falling back to SearXNG when the primary returns 429…"],
  ],
};

export const bashStream = [
  "        PASS [   0.412s] alforria-server routes::v1::session::prompt_async_returns_204",
  "        PASS [   0.388s] alforria-server routes::v1::session::abort_idle_is_noop",
  "        PASS [   0.951s] alforria-server sse::global_event_heartbeat_every_10s",
  "        PASS [   0.207s] alforria-server routes::v2::session::history_reads_projection",
  "        FAIL [   1.104s] alforria-server routes::v2::session::prompt_streams_parts",
  "        PASS [   0.330s] alforria-server routes::v2::session::interrupt_aborts_busy",
  "        PASS [   0.512s] alforria-server middleware::location::session_route_uses_row",
  "        PASS [   0.298s] alforria-server routes::v2::permission::reply_once",
];

// Transcripts. Parts follow the v1 part union: text, reasoning, tool (with
// status), todo snapshots, subtask links, step meta.
export const transcripts = {
  s2: [
    {
      role: "user", at: ago(19),
      text: "The TUI drops its /global/event stream after the laptop sleeps and never reconnects. Find out why and fix it.",
    },
    {
      role: "assistant", agent: "build", model: "glm-4.6", at: ago(19), tokens: 52_800, cost: 0.52, duration: "13m 02s",
      parts: [
        { type: "reasoning", seconds: 9, text: "Reconnect logic lives in transport/events.rs. Backoff caps at 30 s, but a half-open socket never errors. The server heartbeats every 10 s, so a missing heartbeat is the signal to use." },
        { type: "text", text: "I'll check how the client decides a stream is dead." },
        { type: "tool", tool: "grep", target: "heartbeat  crates/alforria-tui", status: "completed", duration: "0.1s", summary: "7 matches in 3 files" },
        { type: "tool", tool: "read", target: "crates/alforria-tui/src/transport/events.rs", status: "completed", duration: "0.0s", summary: "lines 1–140" },
        { type: "text", text: "Found it: the client only reconnects when the connection **errors**. After sleep the TCP socket stays half-open, so no error ever arrives. The server sends `server.heartbeat` every 10 s, so I'll add a watchdog: if no frame arrives for 25 s, drop the stream and reconnect with the existing backoff." },
        {
          type: "tool", tool: "edit", target: "crates/alforria-tui/src/transport/events.rs", status: "completed", duration: "0.2s", add: 9, del: 2, open: true,
          diff: [
            ["hunk", "@@ -41,9 +41,16 @@ pub async fn run_event_loop(client: &Client, tx: Sender<Event>) {"],
            ["ctx", 41, "    let mut backoff = Duration::from_secs(1);"],
            ["ctx", 42, "    loop {"],
            ["ctx", 43, "        let mut stream = client.global_events().await?;"],
            ["del", 44, "        while let Some(frame) = stream.next().await {"],
            ["add", 44, "        loop {"],
            ["add", 45, "            let frame = match timeout(HEARTBEAT_GRACE, stream.next()).await {"],
            ["add", 46, "                Ok(Some(frame)) => frame,"],
            ["add", 47, "                Ok(None) => break,"],
            ["add", 48, "                Err(_) => {"],
            ["add", 49, "                    tracing::warn!(\"no frame for {HEARTBEAT_GRACE:?}; reconnecting\");"],
            ["add", 50, "                    break;"],
            ["add", 51, "                }"],
            ["add", 52, "            };"],
            ["ctx", 53, "            backoff = Duration::from_secs(1);"],
            ["del", 47, "            dispatch(&tx, frame?)?;"],
            ["add", 54, "            dispatch(&tx, frame?)?;"],
            ["ctx", 55, "        }"],
          ],
        },
        {
          type: "todos", items: [
            ["Reproduce: stream silent after suspend", "completed"],
            ["Locate reconnect trigger", "completed"],
            ["Add heartbeat watchdog (25 s grace)", "completed"],
            ["Test reconnect path", "completed"],
            ["Commit and push branch", "completed"],
          ],
        },
        {
          type: "tool", tool: "bash", target: "cargo nextest run -p alforria-tui transport::", status: "completed", duration: "4.8s", open: true,
          output: [
            "    Starting 23 tests across 1 binary",
            "        PASS [   0.031s] alforria-tui transport::events::dispatch_routes_by_directory",
            "        PASS [   0.019s] alforria-tui transport::events::sync_frames_are_dropped",
            "        PASS [  25.004s] alforria-tui transport::events::watchdog_reconnects_after_grace",
            "        PASS [   0.022s] alforria-tui transport::events::backoff_caps_at_30s",
            "────────────",
            "     Summary [  25.413s] 23 tests run: 23 passed, 0 skipped",
          ],
        },
        { type: "tool", tool: "bash", target: "git commit -am \"fix(tui): reconnect when heartbeats stop\"", status: "completed", duration: "0.1s", summary: "[fix/sse-heartbeat 4c1d9e2] 1 file changed, 9 insertions(+), 2 deletions(-)" },
        { type: "text", text: "Tests pass, including a new one that holds the stream silent for 25 s. Pushing the branch so you can open a PR." },
        { type: "tool", tool: "bash", target: "git push origin fix/sse-heartbeat", status: "pending", permission: "q2" },
      ],
    },
  ],

  s1: [
    {
      role: "user", at: ago(38),
      text: "Port the v2 prompt runner so POST /api/session/{id}/prompt stops returning 500. Keep v1 behaviour untouched; fixtures must stay green.",
    },
    {
      role: "assistant", agent: "build", model: "qwen3-coder-480b", at: ago(38), tokens: 81_400, cost: 1.84, live: true,
      parts: [
        { type: "text", text: "Plan: map what the v2 routes already do, find why the `session_message` projection stays empty, then drive v2 prompts through the existing v1 engine instead of porting a second runner." },
        {
          type: "todos", items: [
            ["Map v2 session routes and stubs", "completed"],
            ["Explain empty session_message projection", "completed"],
            ["Adapter: v2 prompt → v1 engine loop", "completed"],
            ["Emit session.next.* events from the adapter", "completed"],
            ["Run server suite; fix regressions", "in_progress"],
            ["Fixture parity: openapi + event manifest", "pending"],
            ["Update docs/plans/opencode-rust-port.md", "pending"],
          ],
        },
        { type: "subtask", session: "s1a", agent: "explore", title: "Find v2 session routes", result: "14 routes; prompt, compact and wait are stubbed in routes/v2/session.rs:634–714" },
        { type: "subtask", session: "s1b", agent: "explore", title: "Audit session_message projection", result: null },
        { type: "reasoning", seconds: 14, text: "If the adapter writes through the v1 store and mirrors into the projection, history/context endpoints light up for free and v1 stays untouched." },
        {
          type: "tool", tool: "edit", target: "crates/alforria-server/src/routes/v2/session.rs", status: "completed", duration: "0.3s", add: 38, del: 41,
          diff: [
            ["hunk", "@@ -634,12 +634,9 @@ async fn prompt(State(app): State<App>, Path(id): Path<SessionId>, Json(body): Json<PromptBody>)"],
            ["del", 634, "    // TODO(M9): v2 runner not ported"],
            ["del", 635, "    Err(ApiError::unknown(\"V2 runner not ported\"))"],
            ["add", 634, "    let input = body.into_v1_input(&app.catalog)?;"],
            ["add", 635, "    let handle = app.engine.prompt_async(id, input).await?;"],
            ["add", 636, "    app.projection.mirror(handle.message_id());"],
            ["add", 637, "    Ok(StatusCode::ACCEPTED)"],
          ],
        },
        { type: "tool", tool: "bash", target: "cargo nextest run -p alforria-server --no-fail-fast", status: "running", duration: "", open: true, stream: true, output: [
          "    Starting 412 tests across 3 binaries",
          "        PASS [   0.140s] alforria-server routes::v1::session::create_returns_info",
          "        PASS [   0.102s] alforria-server routes::v1::session::list_paginates_with_link",
        ] },
      ],
    },
  ],

  s6: [
    {
      role: "user", at: ago(21),
      text: "Users get logged out mid-session when the SSO access token expires. Refresh it transparently.",
    },
    {
      role: "assistant", agent: "build", model: "glm-4.6", at: ago(21), tokens: 46_100, cost: 0.44,
      parts: [
        { type: "tool", tool: "read", target: "src/auth/token.rs", status: "completed", duration: "0.0s", summary: "lines 1–96" },
        { type: "tool", tool: "grep", target: "access_token(  src/", status: "completed", duration: "0.1s", summary: "11 call sites" },
        { type: "text", text: "Every caller goes through `TokenStore::access_token`, so refreshing there covers all 11 call sites. I'll refresh when fewer than 2 minutes remain and persist the new pair." },
        {
          type: "todos", items: [
            ["Find the single token entry point", "completed"],
            ["Refresh inside access_token", "in_progress"],
            ["Persist refreshed pair atomically", "pending"],
            ["Test expiry boundary", "pending"],
          ],
        },
        { type: "tool", tool: "edit", target: "src/auth/token.rs", status: "pending", permission: "q3" },
      ],
    },
  ],

  s3: [
    {
      role: "user", at: ago(12),
      text: "Plan the store for the new web client: one /global/event stream, normalized, SolidJS. Don't write code yet.",
    },
    {
      role: "assistant", agent: "plan", model: "kimi-k2", at: ago(12), tokens: 34_600, cost: 0.21,
      parts: [
        { type: "text", text: "Three facts drive the shape:\n\n- `message.part.delta` arrives far more often than anything else and must apply in constant time.\n- One permission or question has to exist once, however many views render it.\n- The fleet view needs per-session aggregates (status, current tool, todos, cost) without walking transcripts." },
        { type: "tool", tool: "read", target: "crates/alforria-schema/src/event_manifest.rs", status: "completed", duration: "0.0s", summary: "lines 52–232" },
        { type: "question", permission: "q4" },
      ],
    },
  ],

  s9: [
    { role: "user", at: ago(40), text: "Port the /balances endpoint from pyaleph, including the paginated variant." },
    {
      role: "assistant", agent: "build", model: "qwen3-coder-480b", at: ago(40), tokens: 131_000, cost: 1.02,
      parts: [
        { type: "tool", tool: "read", target: "src/aleph/web/controllers/accounts.py", status: "completed", duration: "0.0s", summary: "lines 1–612" },
        { type: "tool", tool: "read", target: "src/api/routes/balances.rs", status: "completed", duration: "0.0s", summary: "lines 1–188" },
        { type: "error", name: "ContextOverflowError", message: "The conversation needs 131k tokens; qwen3-coder-480b accepts 128k. Compact the session or continue in a fork." },
      ],
    },
  ],
};

// Briefs for sessions without a hand-written transcript; the client builds a
// transcript from these plus the session's live state, so a row and its focus
// view always agree.
export const briefs = {
  s1a: {
    prompt: "Find every v2 session route and say which ones are stubbed.",
    tools: [
      { tool: "grep", target: "\"/api/session  crates/alforria-server", duration: "0.1s", summary: "61 matches in 4 files" },
      { tool: "read", target: "crates/alforria-server/src/routes/v2/session.rs", duration: "0.0s", summary: "lines 560–820" },
    ],
    outro: "14 routes. `prompt`, `compact` and `wait` are stubbed in routes/v2/session.rs:634–714; `history` and `context` read a projection the v1 engine never fills.",
  },
  s1b: {
    prompt: "Explain why the session_message projection stays empty under the v1 engine.",
    tools: [{ tool: "read", target: "crates/alforria-core/src/storage/mod.rs", duration: "0.0s", summary: "lines 1–220" }],
  },
  s4: {
    prompt: "Reverting a snapshot loses files that were renamed after it was taken. Fix it.",
    todos: ["Reproduce rename then revert", "Track renames in the snapshot diff", "Restore the old path on revert", "Remove the new path on revert", "Test rename chains", "Run the core suite"],
    tools: [
      { tool: "bash", target: "git diff --name-status -M HEAD~1", duration: "0.1s", summary: "R100  src/a.rs → src/b.rs" },
      { tool: "edit", target: "crates/alforria-core/src/snapshot/revert.rs", duration: "0.3s", add: 27, del: 9 },
      { tool: "write", target: "crates/alforria-core/tests/revert_rename.rs", duration: "0.1s", add: 58, del: 0 },
      { tool: "bash", target: "cargo nextest run -p alforria-core snapshot::", duration: "6.2s", summary: "38 tests run: 38 passed" },
    ],
    outro: "Revert now follows renames: the snapshot diff records `R` entries, and revert restores the old path and removes the new one. Covered by three new tests, including a rename chain.",
  },
  s5: {
    prompt: "Embed the alforria engine in libertai-cli behind a feature flag so `libertai code` runs it in-process.",
    todos: ["Add alforria crates as optional deps", "Gate the engine module behind `alforria`", "Start the embedded server on a free port", "Route `libertai code` to the embedded TUI", "Forward LibertAI auth to the provider config", "Smoke test on Linux and macOS"],
    subtasks: [{ session: "s5a", agent: "general", title: "Audit feature flags" }],
    tools: [
      { tool: "edit", target: "Cargo.toml", duration: "0.1s", add: 6, del: 0 },
      { tool: "bash", target: "cargo check --features alforria", duration: "48.0s", summary: "Finished `dev` profile in 47.81s" },
    ],
  },
  s5a: {
    prompt: "List every cfg(feature) gate under src/ and flag the dead ones.",
    tools: [{ tool: "read", target: "Cargo.toml", duration: "0.0s", summary: "[features] · 9 entries" }],
  },
  s7: {
    prompt: "Write the 0.9 release notes from the PRs merged since 0.8.2.",
    todos: ["Collect merged PRs since 0.8.2", "Group them by area", "Write the CHANGELOG entry"],
    tools: [
      { tool: "bash", target: "gh pr list --state merged --search \"merged:>2026-09-01\"", duration: "1.4s", summary: "27 pull requests" },
      { tool: "edit", target: "CHANGELOG.md", duration: "0.1s", add: 64, del: 0 },
    ],
    outro: "Drafted the 0.9 entry in CHANGELOG.md: 27 PRs in five groups (engine, auth, CLI, docs, CI), with the two breaking flag renames called out first.",
  },
  s8: {
    prompt: "Benchmark message ingestion and get p99 under 20 ms.",
    todos: ["Write a criterion bench for ingestion", "Record the p50/p99 baseline", "Batch inserts per block", "Re-measure", "Document the results"],
    tools: [
      { tool: "write", target: "benches/ingestion.rs", duration: "0.1s", add: 74, del: 0 },
      { tool: "bash", target: "cargo bench --bench ingestion", duration: "1m 52s", summary: "p50 6.1 ms · p99 41.3 ms" },
    ],
  },
  s10: {
    prompt: "Run clippy with pedantic lints and fix whatever is reasonable.",
    todos: ["Run clippy pedantic", "Fix mechanical lints", "Allow the noisy ones with a reason", "Re-run clippy and tests"],
    tools: [
      { tool: "bash", target: "cargo clippy --all-targets -- -W clippy::pedantic", duration: "39.0s", summary: "212 warnings" },
      { tool: "apply_patch", target: "22 files", duration: "1.1s", add: 141, del: 133 },
      { tool: "bash", target: "cargo clippy --all-targets -- -D warnings", duration: "31.4s", summary: "0 warnings" },
    ],
    outro: "Fixed 187 of 212 pedantic warnings across 22 files. The remaining 25 (`module_name_repetitions`, `must_use_candidate`) are allowed at crate level with a comment explaining why.",
  },
  s11: {
    prompt: "Refresh the pricing table with the new per-token prices.",
    todos: ["Read the current pricing data", "Pull new prices from the models API", "Update src/data/pricing.ts", "Rebuild and check the table", "Screenshot for review"],
    tools: [
      { tool: "read", target: "src/data/pricing.ts", duration: "0.0s", summary: "lines 1–212" },
      { tool: "read", target: "src/data/pricing.ts", duration: "0.0s", summary: "lines 1–212" },
      { tool: "read", target: "src/data/pricing.ts", duration: "0.0s", summary: "lines 1–212" },
    ],
  },
  s12: {
    prompt: "Get the homepage's Lighthouse performance score above 90.",
    todos: ["Measure the baseline", "Fix the LCP hero image", "Defer non-critical JS", "Re-measure"],
    tools: [{ tool: "bash", target: "npx lighthouse https://libertai.io --only-categories=performance", duration: "22.7s", summary: "Performance 71 · LCP 3.8 s · TBT 410 ms" }],
  },
  s13: {
    prompt: "Fall back to SearXNG when the primary search provider rate-limits us.",
    todos: ["Add a SearXNG client", "Detect 429 from the primary", "Retry the same query on SearXNG", "Add the HTTP dependency", "Test the fallback path"],
    tools: [{ tool: "read", target: "src/providers/mod.rs", duration: "0.0s", summary: "lines 1–140" }],
  },
  s14: {
    prompt: "Add tests for the rate limiter's refill logic.",
    todos: ["Cover refill at the boundary", "Cover burst then idle"],
    tools: [
      { tool: "write", target: "tests/rate_limiter.rs", duration: "0.1s", add: 48, del: 0 },
      { tool: "bash", target: "cargo test rate_limiter", duration: "3.1s", summary: "6 passed; 0 failed" },
    ],
    outro: "Added 6 tests covering refill exactly at the window boundary and a burst followed by idle time. All pass.",
  },
};

// Generic transcript for sessions without a hand-written one or a brief.
export function fallbackTranscript(s) {
  return [
    { role: "user", at: s.since, text: `${s.title}.` },
    {
      role: "assistant", agent: s.agent, model: s.model, at: s.since, tokens: s.tokens, cost: s.cost,
      parts: [
        { type: "text", text: s.state === "idle" ? `Done. ${cap(s.now.text)}.` : "Working on it." },
      ],
    },
  ];
}
const cap = (t) => t.charAt(0).toUpperCase() + t.slice(1);

export const changes = {
  s2: [
    { path: "crates/alforria-tui/src/transport/events.rs", add: 9, del: 2 },
    { path: "crates/alforria-tui/src/transport/mod.rs", add: 2, del: 0 },
    { path: "crates/alforria-tui/tests/reconnect.rs", add: 31, del: 0 },
  ],
  s1: [
    { path: "crates/alforria-server/src/routes/v2/session.rs", add: 38, del: 41 },
    { path: "crates/alforria-server/src/projection.rs", add: 64, del: 3 },
    { path: "crates/alforria-server/src/routes/v2/mod.rs", add: 4, del: 1 },
  ],
  s4: [
    { path: "crates/alforria-core/src/snapshot/revert.rs", add: 27, del: 9 },
    { path: "crates/alforria-core/src/snapshot/git.rs", add: 12, del: 4 },
    { path: "crates/alforria-core/tests/revert_rename.rs", add: 58, del: 0 },
  ],
};

export const fileTree = [
  { path: "crates/", dir: true },
  { path: "crates/alforria-tui/", dir: true, depth: 1 },
  { path: "crates/alforria-tui/src/", dir: true, depth: 2 },
  { path: "crates/alforria-tui/src/transport/", dir: true, depth: 3 },
  { path: "crates/alforria-tui/src/transport/api.rs", depth: 4 },
  { path: "crates/alforria-tui/src/transport/events.rs", depth: 4, open: true },
  { path: "crates/alforria-tui/src/transport/mod.rs", depth: 4 },
  { path: "crates/alforria-server/", dir: true, depth: 1 },
  { path: "crates/alforria-core/", dir: true, depth: 1 },
  { path: "docs/", dir: true },
  { path: "AGENTS.md" },
  { path: "Cargo.toml" },
];

export const fileContent = `use std::time::Duration;

use futures::StreamExt;
use tokio::{sync::mpsc::Sender, time::timeout};

use crate::transport::{Client, Event};

/// A healthy stream carries server.heartbeat every 10 s; allow 2.5 beats.
const HEARTBEAT_GRACE: Duration = Duration::from_secs(25);
const MAX_BACKOFF: Duration = Duration::from_secs(30);

pub async fn run_event_loop(client: &Client, tx: Sender<Event>) -> anyhow::Result<()> {
    let mut backoff = Duration::from_secs(1);
    loop {
        let mut stream = client.global_events().await?;
        loop {
            let frame = match timeout(HEARTBEAT_GRACE, stream.next()).await {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                Err(_) => {
                    tracing::warn!("no frame for {HEARTBEAT_GRACE:?}; reconnecting");
                    break;
                }
            };
            backoff = Duration::from_secs(1);
            dispatch(&tx, frame?)?;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}`;

export const terminalLines = [
  "~/repos/opencode-rs on main",
  "$ git status --short",
  " M crates/alforria-server/src/routes/v2/session.rs",
  " M crates/alforria-server/src/projection.rs",
  "?? crates/alforria-server/tests/v2_prompt.rs",
  "$ cargo build -p alforria",
  "   Compiling alforria-server v0.1.0 (/home/dev/repos/opencode-rs/crates/alforria-server)",
  "   Compiling alforria v0.1.0 (/home/dev/repos/opencode-rs/crates/alforria)",
  "    Finished `dev` profile [unoptimized + debuginfo] target(s) in 38.21s",
  "$ ",
];

export const settings = {
  providers: [
    { name: "LibertAI", endpoint: "api.libertai.io/v1", auth: "API key · auth.json", models: 41, status: "connected" },
    { name: "Anthropic", endpoint: "api.anthropic.com", auth: "API key · auth.json", models: 9, status: "connected" },
    { name: "OpenAI", endpoint: "api.openai.com/v1", auth: "Not configured", models: null, status: "not connected" },
    { name: "OpenRouter", endpoint: "openrouter.ai/api/v1", auth: "Not configured", models: null, status: "not connected" },
  ],
  permissions: [
    { tool: "bash", pattern: "*", action: "ask" },
    { tool: "bash", pattern: "git status*", action: "allow" },
    { tool: "bash", pattern: "cargo test*", action: "allow" },
    { tool: "bash", pattern: "rm -rf *", action: "deny" },
    { tool: "edit", pattern: "*", action: "ask" },
    { tool: "webfetch", pattern: "*", action: "allow" },
    { tool: "external_directory", pattern: "*", action: "ask" },
  ],
};

# Improvement plan: web client, TUI and the server boundary

Source: the full review of 2026-10-12. Nine reviewers covered web (data layer, focus
view, shell/dialogs, security/UX), TUI (state/transport, rendering/dialogs,
keymap/app loop), and parity against opencode `88c6c7ab` for both clients. Every
finding was traced in code; the starred ones (★) were also re-checked by hand or
reproduced with a throwaway test. Paths are relative to `web/src` (web) or
`crates/alforria-tui/src` (TUI) unless given in full.

Each milestone is one commit (chunked-factory discipline; `fixup:` for review fixes).
Phases are ordered by risk: security, then crashes and data loss, then correctness,
then polish, then features. Milestones inside a phase are independent and can land
in any order.

## Phase 0: the server boundary (security)

### S1 Lock down who can talk to the server
- Work:
  - ★ Reject requests whose `Host` is not loopback (`localhost`, `127.0.0.1`,
    `[::1]`), the bound `--hostname`, or a `--cors` origin's host, whenever no
    password is set. DNS rebinding currently passes `same_host` and can spawn PTYs
    (`crates/alforria-server/src/middleware/cors.rs:64`).
  - ★ Send `Content-Security-Policy: frame-ancestors 'none'` and
    `X-Frame-Options: DENY` with the UI (`routes/ui.rs`, `csp_for_html`), so it
    can't be framed and click-jacked (read delay covers keys, not clicks).
  - Narrow CORS to the server's own origin plus `--cors` (`cors.rs:19-45`). alforria
    has no relationship with opencode's hosted or desktop clients, so drop every
    upstream origin: `*.opencode.ai`, `tauri://localhost`, `http(s)://tauri.localhost`,
    `oc://renderer`, and `http://localhost:*` / `127.0.0.1:*` on other ports.
    Record as a deliberate deviation in `review-findings.md` (decided 2026-10-12).
  - Make the PTY websocket's `?ticket=` exemption validate the ticket in the auth
    middleware instead of deferring to `pty/ws.rs` (`middleware/auth.rs:281`).
- Accept: integration tests for a rebound Host (403), a framed load (headers
  present), a foreign `localhost:<other port>` origin (no ACAO), and the existing
  embedded e2e staying green.

### S2 The TUI runs its server in-process (decided 2026-10-12)
- Work: ★ plain `alforria` binds an unauthenticated loopback listener
  (`crates/alforria/src/cmd/tui.rs:236-260`); upstream runs the server in-process
  with no socket. Do the same:
  - a transport seam in the TUI (`transport/api.rs`, `transport/events.rs`) with
    two backends: HTTP (for `attach` and `--port/--hostname/--mdns`) and
    in-memory, calling the server's axum router as a tower `Service`, SSE
    included;
  - plain `alforria` builds the router and opens no listener.
  - Fallback if the in-memory SSE path proves costly: a `0600` Unix socket (no
    browser can reach it). Decide inside the milestone, by measurement.
  - Use the listener's real address when `--hostname` is a specific IP (`tui.rs:243`).
- Accept: plain `alforria` opens no TCP port (`ss -ltnp`); the full TUI e2e suite
  passes on both backends; `attach` unchanged.

### S2b `alforria serve` has a password by default (decided 2026-10-12)
- Work: without `OPENCODE_SERVER_PASSWORD`, `serve` generates a random password,
  prints the URL, username and password at startup, and the browser's Basic-auth
  prompt does the rest. `bun run server` and the e2e scripts pass a fixed dev
  password; `attach --password` already exists. Deliberate deviation (TS only warns).
- Accept: unauthenticated requests to a default `serve` get 401; the embedded e2e
  logs in and stays green.

### S3 Dev server stays local
- Work: vite binds `127.0.0.1` by default; LAN only with an explicit flag, matching
  `bun run server --lan` (`web/vite.config.ts:57,69`). Its Origin rewrite currently
  hands any LAN client an unauthenticated shell.
- Accept: `bun run dev` is unreachable from another host without the flag.

## Phase 1: crashes and data loss

### T1 TUI rendering correctness
- Work:
  - ★ `reasoning_summary` slices bytes and panics on any non-ASCII bold title
    (`ui/session/parts.rs:101`); port the TS regex.
  - ★ Diff parser: skip everything before the first `@@` (core emits `Index:` and
    `====` headers that render as junk rows); treat `---`/`+++` as headers only
    outside a hunk; ignore `\ No newline`; drop the byte `split_at(1)`
    (`ui/diff.rs:38-80`).
  - Display-width math: `wrap_spans`, table widths and the prompt cursor count
    chars, not cells, so CJK wraps at 2× width and the cursor lands wrong
    (`ui/markdown.rs:466,535,708-760`, `ui/session/prompt.rs:494-509`,
    `ui/home.rs:666-678`). Fix the scrolled-textarea phantom cursor too.
  - Table fit scales with border overhead included and overflows
    (`ui/markdown.rs:473-479`).
  - Permission prompt clips its option row under long diffs; truncate only the
    body (`ui/session/permission.rs:572-620`).
  - Reasoning body double-indented (`parts.rs:205-211`); "Thought · 2.0s" label
    (`parts.rs:185`); expand tabs before measuring; CommonMark flanking for `*`.
- Accept: unit tests for each (CJK paragraph at width 10, an edit diff from core,
  `**思考**`, a 12-line permission diff at 24 rows); a fuzz sweep of `ui::view`
  with mixed-width text finds no panic.

### W1 Web transcript and snapshot stability
- Work:
  - ★ The focus effect reads `state.sessions[sid]`, so every `session.updated`
    releases and re-fetches the open transcript: "Loading…" flash on every send,
    lost scroll, lost expanded rows and half-typed answers (`views/focus/focus.tsx:92`).
    Key the effect on the id only.
  - ★ `resync` awaits every open transcript with no catch: a session deleted while
    offline 404s and the whole fleet resync retries forever (`sync/sync.ts:51`).
    Load each with its own catch; on 404 drop the session and close the column.
  - One failing directory fails the snapshot (`Promise.all`); use `allSettled`
    per directory, fail only on `/project` or the per-project list.
  - Keep sessions that have viewers (not just `loaded`) across a snapshot; drop
    message/part/todo/diff/error data for sessions that left it.
  - `loadRecentActivity` writes outside `withSnapshot` and blanks streaming text;
    route it through `withSnapshot` with the streamed-text merge.
  - Handle `server.instance.disposed` (re-snapshot that directory; drop cached
    agents/commands).
  - Never query `directory=/` for the non-git project; use the directories of its
    recent sessions only. Same for `loadModels`, settings' `anyDir`.
  - `scope=project` caps at 100 per project, not per directory; pass
    `start = now − HISTORY_WINDOW` and fix the comments.
- Accept: reducer/sync tests for delete-while-offline, a failing directory, a
  `session.updated` while a transcript is open (no refetch), and an instance
  disposal; `bun run e2e` green.

### W2 Revert that can't destroy history
- Work:
  - ★ "Revert all" in a non-git project restores nothing (no snapshots) yet toasts
    success, and the next prompt's cleanup deletes the history
    (`views/focus/changes.tsx:103-118`). Hide or disable it when the server can't
    restore files, and say why.
  - Render `session.revert`: dim or hide turns at and after `revert.messageID`, a
    dock with Unrevert (`api.unrevert` exists, unused) and "restore prompt to
    composer" (upstream `session-revert-dock.tsx`).
  - Stop a busy session before reverting (upstream `halt → revert.stage`).
  - Changes: count `apply_patch` results (`metadata.files[]`) in the edit fallback;
    validate the picked source against the rendered options (`changes.tsx:53,140,227`).
- Accept: e2e: revert, see the dimmed turns, unrevert, revert again and prompt;
  files on disk match each step; a non-git session offers no destructive revert.

## Phase 2: correctness

### W3 Keyboard, queue and dialogs
- Work:
  - ★ Overview Enter is captured globally, so focused buttons only work with Space
    (`nav/keys.ts:61`). Act only when focus is on the body, table or a row.
  - Queue Enter/A/S/D/X act only when the target is the slip itself, not a button,
    link or summary inside the queue (`views/queue/queue.tsx:86,104`).
  - ★ Sort interrupts by the id body (`id.slice(4)`), not the raw id: every
    `per_` sorts before every `que_` (`fleet/derive.ts`, `queue.tsx:19`).
  - Keep a replied set until the item leaves `interrupts()`, so a slow SSE can't
    resurface it for a second reply (`queue/act.ts:88`).
  - ★ Single-choice question: custom text replaces the choice instead of being sent
    alongside it (`queue/slip.tsx:239`). Share question drafts per request between
    the queue slip and the inline slip.
  - Modal focus trap: exclude `tabindex="-1"` from `FOCUSABLE` (`ui/modal.tsx:15`).
  - Toasts render outside `#app` so they're announced while a dialog is open.
  - Palette: drop stale `findFiles` responses.
  - Stop no longer shows as fault (ignore `MessageAbortedError` in `session.error`).
  - Context meter uses the last assistant with output tokens; notifications seed
    `known` from the first applied snapshot (no burst on load).
  - OAuth: a `disposed` flag so a closed Connect dialog can't act later
    (`views/connect.tsx:55-101`).
- Accept: Playwright keyboard checks (Enter on each overview button, Enter on queue
  controls, Tab wraps in palette and folder dialog), a question with custom text
  sends one answer, a mixed permission/question queue is in age order.

### W4 Composer, files and settings
- Work:
  - Forward the session's model `variant` on prompt and command; type `variants`
    in `ProviderList` (`views/focus/composer.tsx:151-183`).
  - Attachments: allow-list text types, refuse other binaries; send file parts with
    slash commands; encode `file://` mention URLs per segment and widen the `@`
    regex (`composer.tsx:29-33,166-182`).
  - Enter inserts a newline on coarse pointers; ignore IME commits (`keyCode 229`).
  - Show Stop whenever the server status is busy, whatever the draft or fleet state.
  - Transcript follow also observes the scroller (composer growth, phone keyboard).
  - Files: error state for failed reads; don't cache failed listings; reveal only
    when the file changes so folders can collapse; refresh on `file.watcher.updated`.
  - PTY: replay from 0 if the previous socket never synced.
  - Settings: the `*` permission row writes `{tool: {"*": action}}` when the tool
    has patterns; "Server default" model needs a real clear (open decision 3) and
    no success toast when nothing changed (`views/settings.tsx:205,317`).
- Accept: unit tests for request bodies (variant, file parts, encoded URLs, the
  permission object); e2e: attach a .zip is refused; collapse a folder holding the
  open file.

### W5 Open folder: every past folder, browsing without side effects (decided 2026-10-12)
- Work:
  - ★ Browsing calls `GET /file?directory=<each folder>`, which boots an instance and
    upserts a project (with sandboxes) for every git folder stepped into. List with
    v2 `GET /api/fs/list` anchored at one base instead: `location[directory]` is the
    server's home (or `/` for paths outside it) and `path` is the absolute folder;
    the handler only refuses paths outside the base (`routes/v2/misc.rs:731-756`),
    so one instance serves every listing and no project rows are created.
  - Recent lists every folder ever used: the distinct directories of
    `GET /experimental/session` (all projects, paged by cursor) plus every project
    worktree, newest first, not just the 3-day fleet window.
  - Enter race: show a pending "Start here" row until the listing lands, and don't
    default to Recent after navigating (`views/open-folder.tsx:97-134`).
  - Cache listings per dialog, not per page; the header's project count counts
    shown groups (`app.tsx:181`).
- Accept: browsing five git folders adds no project rows (`GET /project` unchanged);
  a folder last used a month ago is in Recent; type a path, Enter, Enter starts in
  that folder.

### T2 TUI terminal handover and the event loop
- Work:
  - One suspend/resume helper for `$EDITOR`, export and ctrl+z: pause the input
    pump, stop drawing, pop kitty flags, disable mouse and bracketed paste, then
    restore and `terminal.clear()` (`lib.rs:942-963,1039-1066,1151-1161`).
  - Panic hook and SIGTERM handler that restore the terminal; SIGHUP sets exit
    directly (`lib.rs:1086-1142`).
  - Never hold the app lock across HTTP: fetch, then apply under a short lock
    (bootstrap, session sync/mount/hydrate, MCP toggle); set `Partial` after phase 1
    so a slow MCP server can't freeze startup (`lib.rs:400-407,718-886`,
    `state/sync.rs:837,968`).
  - Redraw only when something changed; tick only while animating; drain queued
    messages before a draw (`lib.rs:168-198`).
  - Cache rendered transcript lines per part (content version, width, theme,
    toggles) and render only the visible window (`ui/session/transcript.rs:124-181`).
  - Kv lock wait off the UI path (`state/kv.rs:259`); `append_delta` fast path for
    text and reasoning (`state/sync.rs:337`).
- Accept: idle CPU near zero; a 400 KB transcript draws under 2 ms per frame; a
  stalled MCP server leaves the TUI responsive; vim opened from the prompt gets
  every key and the TUI repaints on return.

### T3 TUI input routing, local state and transport
- Work:
  - Autocomplete consumes the keys it handles (Enter no longer submits, ctrl+p no
    longer opens the palette, Esc no longer interrupts) (`state/mod.rs:828-873`).
  - `Char(' ')` maps to `space` so space bindings fire (`keymap/bindings.rs:1279`).
  - Paste goes to the focused question/permission field (`state/mod.rs:973`).
  - App-global bindings work inside dialogs, as in TS (`state/mod.rs:813`).
  - Shell mode resets on submit, not after the command returns
    (`state/prompt.rs:1484`).
  - History/stash keep the newest 50 and never rewrite on an empty or failed parse;
    keep `FileSource` as raw JSON so TS-written entries survive
    (`state/prompt.rs:343,511,48-53`).
  - Clipboard: image paste decodes before re-encoding; `-t image/png` for wl-paste
    and xclip, `osascript` on macOS; copy checks exit status, gates `wl-copy` on
    `WAYLAND_DISPLAY`, adds `xsel` (`lib.rs:917`, `clipboard.rs`).
  - Diagnostics go to the log, not `eprintln!` over the alternate screen; a missing
    `kv.json` is silent (`state/kv.rs:327,391`, `state/local.rs:669`).
  - Sanitize OSC 9 notification titles like TS `normalizeText` (`attention.rs:56`).
  - Percent-encode path segments (`transport/api.rs`).
  - After an SSE reconnect re-fetch status and re-hydrate the routed session; add a
    heartbeat watchdog (`state/sync.rs:419`, `transport/events.rs:255`).
- Accept: key-routing tests for each popup case; history round-trip with a
  TS-written file of 60 entries; a reconnect test that ends with fresh status.

### T4 TUI config and branding
- Work:
  - ★ Load the user's TUI config (global `tui.json`, `OPENCODE_TUI_CONFIG`, project
    and `.opencode` files, legacy keys migrated out of `opencode.json`) instead of
    `TuiConfig::default()` (`crates/alforria/src/cmd/tui.rs:41`). Honour
    `scroll_acceleration` and `cursor`.
  - User-facing strings say `alforria`; drop tips for commands that don't exist
    (`ui/home.rs:411-494`, `ui/dialogs/system.rs:269`).
- Accept: a fixture `tui.json` remaps a key, sets a theme and disables the mouse,
  and all three take effect.

## Phase 3: UX polish (web)

### W6 Phone, keyboard reach and contrast
- Work:
  - Phone targets ≥ 40px: filter segments, Open folder, composer agent/model/attach,
    Sign in, slip session link.
  - Rail: focusable listbox with roving tabindex or `aria-activedescendant`; no
    buttons inside `role=option`.
  - Light theme: ink-2 on `plate-2` (ink-3 is 4.31:1); added-row line numbers.
  - Provider rows on phones; settings nav scrolls to the current page.
  - Success shown in place, not toasted (`connect.tsx:61`, `settings.tsx:181`,
    `changes.tsx:114`).
  - `groups()` indexes children by parent once (O(n) per run).
- Accept: axe clean in both themes; the e2e target-size check covers every control.

## Phase 4: features (parity where it fits)

Ranked by how much a user misses them. Each is one milestone.

Web:
1. **F1 Session management**: rename, archive (with an archived view), delete. The
   client calls exist and are unused.
2. **F2 Model variants**: picker and cycle, restored from the last user message.
3. **F3 Prompt history and persistent drafts**, plus **shell mode** (`!` →
   `POST /session/:id/shell`).
4. **F4 Fork from a message**, copy a response, export a transcript, message
   navigation keys.
5. **F5 Notifications** for done and failed turns (not only waiting), unseen
   markers and "next unseen".
6. **F6 Per-session auto-accept** behind Confirm-to-Loosen.
7. **F7 Queued follow-ups** while a session is busy.
8. **F8 Worktrees**: create, remove and start a session in one.
9. **F9 Paging**: transcript cursor paging; an "All sessions" search beyond the
   history window.
10. **F10 Per-directory catalogs**: model list, MCP and LSP status per project.
11. **F11 Review comments** on diff and file lines sent as prompt context.

TUI:
1. **F12 `@` mentions**: files and directories with `#Lx-y` ranges (server has
   `GET /api/fs/find`), MCP resources (already synced), frecency.
2. **F13 Mouse text selection** with copy-on-select and ctrl+y.
3. **F14 Server-side session search** in the session list.
4. **F15 Custom themes** from `themes/*.json`; a real terminal palette probe for
   `system`; wire the dead `workspace.list` dialog.

Still non-goals (recorded in `review-findings.md`): diff viewer route, which-key
panel, TUI plugins, IDE editor context, `--mini`.

## Server follow-ups seen through the clients

- `/global/event` drops frames on broadcast `Lagged` with no signal; emit a resync
  marker so clients re-snapshot.
- `vcs.branch.updated` is never emitted; branch labels go stale.
- `InstanceStore` never evicts; idle folders stay resident (upstream: 60-minute TTL).
- Already known: `session.diff` always empty, `file.edited` never emitted.

## Open decisions

1. **Clearing a config key.** `PATCH /global/config` merges, so "Server default"
   can't remove `model` (the body serializes to `{}`). Proposed: the server treats
   `null` as "delete this key" (as it already special-cases `shell: ""`), which
   lets every setting be cleared; the alternative is hiding the option.

Decided 2026-10-12: folder picker (W5), in-process TUI (S2), password by default for
`serve` (S2b), dropping every upstream CORS origin (S1).

## Suggested order

S1, S2b, S3 → T1, S2, W1, W2 → W3, W5, T2, T3, T4, W4 → W6 → F1, F2, F12, F3, F5, then the
rest by demand. S1, S2b and T1 are small and remove the worst risks; S2 is larger
and follows. W1 also fixes the most visible web glitch (the reload on every send).

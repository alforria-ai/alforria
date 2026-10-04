# Web client: a new in-tree UI built for supervising a fleet

Status: **landed 2026-10-03** (W1–W7). The new client is the UI embedded in
`alforria serve`. Supersedes the fork-and-grid approach in `docs/plans/web-ui.md`
(M5/M6). Open follow-ups are listed at the end.

Design source of truth:
- the prototype in `web/prototype/` (served with `python3 -m http.server` from that
  directory);
- its brief, `.impeccable/surfaces/web.md`;
- `DESIGN.md`, written at the end of the design round.

## Why replace the fork

The forked upstream app (`alforria-ai/web`) was built around one session per route.
Our grid bolted tiling onto it by putting a full copy of the app in an iframe for
every tab. That causes most of the failures:
- panes reload when you switch tabs or drag nested splitters;
- splitter drags get stuck over iframes;
- a subagent link opens a grid inside the pane;
- every iframe has its own event stream and copy of settings and permission state,
  so prompts can be answered twice and notifications multiply;
- narrow panes switch to the mobile layout.

On top of that we carry about 159k lines of upstream code with no rebase path.

LibertAI Desktop had good ideas but a renderer we can't port: a 23k-line `app.js`,
whole-tree re-renders, and pi_agent_rust's event model. We keep its ideas as a
checklist of capabilities, not as a template.

## Decisions (user, 2026-10-03)

| Topic | Decision |
|---|---|
| Approach | New client written in-tree at `web/`, as one page with one event stream and one store |
| Layout | Overview + focus. The overview is an **interrupt queue** (everything waiting on you, oldest first, cleared from the keyboard) plus a **session table** (one live row per session, grouped by project, subagents nested). Focus shows 1–3 columns: session, changes, files or terminal |
| Look | The alforria datasheet identity: square corners, hairline rules, Barlow Condensed caps, mono only for data, orange `#E25303` only where the human is needed. Dark first, light theme matching the website |
| Desktop | Web first, but ready to be wrapped in a desktop shell: a thin platform layer that a Tauri shell can implement later |
| Navigation | Overview and Focus are tabs in the top band, with the current one inverted; focus has a visible "← Overview" (Esc), settings has "← Back". In focus, the left panel is a live **session list** (every session, grouped by project): click swaps the active column, shift-click or "open beside" adds a column, Alt+↑/↓ steps, and "N waiting · open next" (W) jumps to the oldest blocked session. Opening from the table replaces the active column by default. Every view has a URL (`#/`, `#/focus/<ids>`, `#/settings/<page>`), so browser Back/Forward work and reload keeps columns. On phones: Queue / Sessions / Focus tabs, and ← back from a session returns to the list (view and tab always agree) |
| Hierarchy | project → session. A LibertAI cloud product can later add account → machines above it through an injectable server-connection slot |
| Scale | A fleet of 7+ sessions. Used from a laptop over the LAN, on localhost, on a big monitor and on a phone |
| v1 scope | Chat, review changes (diff + revert), terminals, file viewer, settings |

## Architecture

```
web/                         bun + Vite + SolidJS + TypeScript
  src/api/                   typed client generated from /openapi.json (openapi-typescript)
  src/sync/                  one EventSource on /global/event → reducers → store
  src/store/                 normalized: sessions, messages, parts (flat by id),
                             permissions, questions, todos, diffs, pty
  src/fleet/                 derived per-session aggregates (state, now, todos, ctx, cost)
  src/views/                 band, queue, table, focus/{session,changes,files,terminal},
                             settings, palette, keys
  src/ui/                    datasheet primitives (stamp buttons, plates, spec rows,
                             lamps, diff, meters) + tokens.css
  src/platform/              web impl of: notify, openExternal, pickFolder, clipboard
  prototype/                 the approved design prototype (reference, not shipped)
```

### Wire

- **API version.** Use v1 only. v2's prompt, history and revert endpoints are still
  stubs.
- **Project scoping.** Send `?directory=` on project-scoped calls.
- **Auth.** HTTP Basic auth is opt-in. When it is on, `EventSource` sends
  `?auth_token=` because it cannot set headers.
- **Event stream.** One `/global/event` stream, which already covers every
  directory:
  - ignore `sync` frames;
  - a watchdog reconnects when no frame arrives for 25 s (heartbeat is 10 s);
  - backoff starts at 1 s, doubles and caps at 30 s, matching the TUI.
- **Resync on reconnect.** Refetch `GET /session/status`, `/permission` and
  `/question`, and the tail of every open transcript (`/session/{id}/message?limit=`).
- **Permissions and questions.** Each exists once in the store, keyed by id. The
  queue, the table row and the inline slip all render that same record. Replies go
  to `POST /permission/{id}/reply` (`once` / `always` / `reject`) and
  `POST /question/{id}/reply` or `/reject`.
- **Prompts.** Send through `POST /session/{id}/prompt_async`; results arrive over
  SSE.
- **Streaming.** `message.part.delta` appends to `part[field]` in O(1). Parts live
  in a flat map, and each message keeps an ordered list of part ids.
- **Terminals.** Mint a ticket with `POST /pty/{id}/connect-token` (one use, 60 s),
  then open the WebSocket at `/pty/{id}/connect?ticket=`. Resize with `PUT /pty/{id}`.

### Rendering

- **Reactivity.** Solid's fine-grained updates mean a streaming delta re-renders
  one text node, never a pane. Columns are keyed by id; nothing is an iframe.
- **Long transcripts.** Virtualize them with `@tanstack/solid-virtual`, and keep
  the stick-to-bottom and "Latest" behaviour from the prototype.
- **Markdown and code.** Render markdown in a worker. Highlight with Shiki, loading
  language grammars lazily and using a monochrome datasheet theme. Diffs use
  `diff` (jsdiff) into our own diff component: soft-wrapped in slips,
  horizontally scrolling in the Changes view.
- **Terminal.** Use xterm.js. ghostty-web is the alternative; pick by bundle size
  and input fidelity.
- **Bundle budget.** Under 300 KB gzipped of initial JS, with grammars and the
  terminal loaded lazily. Today's embedded UI is 9 MB compressed.

### Shipping

`bun run build && bun run pack` in `web/` writes
`crates/alforria-ui/assets/ui.tar.zst`, as today. The Rust build never needs bun,
and `EmbeddedUiBackend` is unchanged.

## Milestones

Each milestone is one commit, following the chunked-factory discipline.

1. **W1 Foundation.**
   - Work: scaffold `web/`; generate types from `/openapi.json`; build the event
     stream (watchdog, backoff, resync), the normalized store and reducers; embed.
   - Accept:
     - reducer unit tests replay recorded event fixtures into the expected store
       state;
     - the shell renders against a real `alforria serve` with zero console errors.
2. **W2 Queue and session table.**
   - Work: band counter, interrupt queue (stamp, fold, cleared ledger, all-clear),
     session table with live rows; keyboard map.
   - Accept: Playwright against `alforria serve` with the scripted backend from
     `crates/alforria/tests/e2e_agent/fixtures`. A permission raised by the engine
     appears once, is cleared with `A`, the engine continues, and the counter
     decrements.
3. **W3 Session focus.**
   - Work: transcript (all part types, tool previews, todos, subtasks, fault
     recovery), composer (`@` files via `/find/file`, `/` commands via `/command`,
     agent and model switch, abort), 1–3 columns.
   - Accept: a scripted multi-step session renders the same as the TUI's recorded
     state, and streaming deltas never re-render the column.
4. **W4 Changes, files, terminal.**
   - Work: `/session/{id}/diff` review with revert/unrevert; file tree and viewer;
     PTY columns.
   - Accept: revert restores the snapshot and the e2e check confirms it on disk;
     the PTY echoes and resizes.
5. **W5 Settings and palette.**
   - Work: providers/auth, models, agents, MCP, permissions, appearance, server;
     the ⌘K palette.
   - Accept: every setting round-trips through `PATCH /config`.
6. **W6 Phone and polish.**
   - Work: queue, sessions and focus tabs; a11y pass (WCAG AA, full keyboard,
     reduced motion); performance with 20 sessions × 2k parts.
   - Accept: the impeccable finish review returns **ship**, and the bundle budget
     holds.
7. **W7 Cutover.**
   - Work: replace the embedded bundle; mark `web-ui.md` superseded; archive
     `alforria-ai/web`.
   - Accept: `alforria serve` serves the new client, and the CI fmt, clippy and
     test gates stay green.

## Status (2026-10-03)

| Milestone | State | Notes |
|---|---|---|
| W1 Foundation | landed | typed client, `/global/event` with watchdog/backoff/resync-with-retry, normalized store, replay tests over four real recordings |
| W2 Queue + table | landed | stamp/fold, read delay against accidental approvals, filters, stable order |
| W3 Session focus | landed | id-keyed transcript (no remounts), markdown + Shiki, inline slips, composer with @files, /commands, attachments |
| W4 Changes, files, terminal | landed | Changes falls back to the session's edit diffs while `session.diff` is empty server-side; xterm PTY columns |
| W5 Settings + palette | landed | providers/auth, models, agents, MCP, permissions (effective defaults, confirm-to-loosen), appearance, server |
| W6 Phone + polish | landed | tab/pane agreement, 40px targets, 16px inputs, axe-core clean, 20 sessions × 2k parts in ~90 ms through the reducer, design review fixes |
| W7 Cutover | landed | `ui.tar.zst` 517 KB (was 9 MB); embedded e2e 26/26; docs point at `web/` |

Gates: `bun run check` (prettier, tsc, vitest: 51 tests), `bun run e2e` (preview build,
24 checks incl. axe) and `E2E_EMBEDDED=1 bun run e2e` (the binary itself, 26 checks).

Follow-ups outside the client:
- ~~Archive `alforria-ai/web`~~: archived 2026-10-04, its description pointing here.
- Server gaps seen through the UI: `session.diff` is always empty and `file.edited`
  is never emitted; open text parts read empty over REST until they finish; v1 can't
  see exited PTYs (exit codes come from v2).
- Fixed on the way: cross-site writes are now refused by the CORS middleware
  (`POST /pty` from any website could spawn a command when no password was set).

## Open decisions

- xterm.js or ghostty-web for terminals (decide in W4 by measurement).
- Icon set: keep the authored 1.5-stroke SVG sprite, or adopt a library drawn in
  the same stroke.
- Whether `web/prototype/` stays in the tree after W3, or moves to a tag.
- The LibertAI cloud layer (machine list, relay, sign-in) is a separate project
  that builds on the server-connection slot.

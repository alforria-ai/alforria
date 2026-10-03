# Web UI: reuse upstream `packages/app`, embed it, improve it

> **Superseded (2026-10-03)** by `docs/plans/web-client.md`: the fork in `alforria-ai/web` and its iframe grid are replaced by a new in-tree client in `web/`. This document is kept for history.

Status: M1 (fork + build + pack), M2 (embed + serve), M3 (branding/theme),
M4 (wire parity) and M5 (multi-pane grid) **landed**; continuing on UX polish.

- `alforria-ai/web` @ `953abeb` holds the fork, `script/pack.ts`, and branding:
  alforria wordmark, `<title>`/manifest, an `alforria` theme (cyan accent,
  derived from oc-2) now the web default, home-route wordmark, app-name i18n
  strings, and regenerated favicon/apple-touch/PWA icons + social-share card.
  The upstream bun `patches/` are vendored and registered via
  `patchedDependencies` (notably `@pierre/trees`, `solid-js`) — without them the
  tree/expansion tests and dialog typecheck fail.
- **Multi-pane grid (M5):** a grid mode tiles session and terminal panes in an
  arbitrary nested split tree, independent of the tab strip. Panes are resizable
  (drag any divider), splittable right/down, maximisable, closable, and
  drag-to-reorderable; layout + sizes persist per server (`grid.<serverKey>`).
  Each pane is a same-origin iframe onto a chrome-free embed route — sessions use
  `/server/:serverKey/session/:id?embed=1`, terminal panes use
  `/server/:serverKey/terminal/:id?embed=1` (a workspace-scoped PTY anchored to a
  session for directory resolution). Embed mode hides the titlebar/tab strip and
  suppresses shared tab-store mutations; grid mode hides the tab strip. The grid
  toggle lives in the titlebar. The first flat schema migrates to a row split.
- `crates/alforria-ui/assets/ui.tar.zst` (8.9 MB, `OPENCODE_CHANNEL=prod`) is
  embedded and served via `EmbeddedUiBackend`; `OPENCODE_DISABLE_EMBEDDED_WEB_UI=1`
  reverts to the empty backend.
- Verified end to end: `alforria serve` + headless Chrome renders the real SPA
  against the Rust server (home + session routes, live history/tool blocks/tabs/
  composer, `/global/event` SSE), every request 200, zero console errors, no DEV
  badge, `alforria` title/manifest, cyan accent, wordmark on home. The grid was
  verified with two live session panes, a mixed session+terminal grid, drag
  resize, pane close, and the add-pane menu.
- `packages/app` unit suite green (728 pass); `crates/alforria-ui` + server lib
  tests green, `cargo fmt`/clippy clean.

Next: M5+ — UX iteration (grid niceties: vertical splits, drag-to-reorder,
full-screen pane; all presentation-only and wire-compatible).

Reference: upstream `anomalyco/opencode` @ `88c6c7abc7f320b6aabed2634ac0b2d6e6ecea67`
(the pinned commit in `fixtures/PINNED.md`). Local clone: `/tmp/opencode-src`.

## Goal

`alforria serve` should serve a real, working web UI at `/`, built from a
**forked copy of upstream `packages/app`** that we own and can improve. Today the
Rust server only has the serving skeleton (`crates/alforria-server/src/routes/ui.rs`)
wired to `EmptyUiBackend`, so every UI request 404s.

## Findings that shape the design

- The web UI is `packages/app` (SolidJS + Vite + Tailwind), **not** `packages/web`
  (that is the Astro docs site). It builds in ~12s:
  `OPENCODE_CHANNEL=dev bun run --cwd packages/app build`.
- Output `packages/app/dist`: **34.4 MB / 952 files** (no `.map`). Main JS 2.7 MB,
  plus code-split Shiki language chunks, wasm, ghostty-web, fonts, icons.
- Upstream embeds it by generating `opencode-web-ui.gen.ts` (path → `import …
  with {type:"file"}`) that Bun bakes into the binary; the server dynamic-imports
  it in `packages/opencode/src/server/shared/ui.ts`.
- The app's `@opencode-ai/*` import closure is small and forkable:

  | pkg | app imports | use |
  |---|---|---|
  | `@opencode-ai/ui` | 599 | `packages/ui`, 241 src files |
  | `@opencode-ai/session-ui` | 48 | `packages/session-ui`, 95 src files |
  | `@opencode-ai/sdk` | 100 | `packages/sdk/js`, 39 src files, self-contained (only `cross-spawn`) |
  | `@opencode-ai/client` | 41 | already a vendored tgz (`app/vendor/opencode-ai-client-1.17.13-v2.tgz`) |
  | `@opencode-ai/core` | 5 pure utils (`util/{array,binary,encode,path,retry}`) | shim: `util/` only; drop the rest (AI SDK/effect/sqlite/pty) |
  | `@opencode-ai/schema` | `schema/event` | vendor `packages/schema/src` (64 files, effect-only) |

- The app auto-detects the server protocol (`src/utils/server-protocol.ts`):
  `/global/health` healthy ⇒ **v1**; `/api/health` with numeric `pid` ⇒ v2;
  else default v2. Our server serves both, so the app will select **v1**;
  the same is true of upstream opencode against itself, so this is expected.

## Architecture: separate repo + committed artifact

The fork lives in a **new private repo `alforria-ai/web`**, not in this tree, so
the JS toolchain and upstream rebases stay isolated. `alforria` consumes a
pinned, compressed build artifact.

```
alforria-ai/web  (bun workspace, mirrors upstream's packages/ tree)
  packages/app/                # forked packages/app
  packages/ui/                 # forked packages/ui
  packages/session-ui/         # forked packages/session-ui
  packages/sdk/js/             # forked packages/sdk/js  (@opencode-ai/sdk)
  packages/schema/             # forked packages/schema  (src only)
  packages/core/               # shim: name @opencode-ai/core, src/util/* only
```

Mirroring upstream's relative paths keeps `workspace:*`, `@/*` aliases, and
`file:../app/vendor/…` references byte-identical, so almost no config edits are
needed. `node_modules/` and `dist/` are gitignored.

Artifact flow:

```
alforria-ai/web: bun run build -> packages/app/dist -> script/pack.ts -> ui.tar.zst
alforria:        crates/alforria-ui/assets/ui.tar.zst   (committed, ~compressed)
                 build.rs embeds it; EmbeddedUiBackend decompresses in-memory
```

Rebuild is one documented step: build in `alforria-ai/web`, run the pack script,
copy `ui.tar.zst` into `crates/alforria-ui/assets/`, `cargo build`.

## Milestones

### M1 — Fork & build (`alforria-ai/web`)
Fork the closure above into a fresh private repo `alforria-ai/web`; `bun install`
+ `bun run build` produces `packages/app/dist`. Add `script/pack.ts` emitting
`ui.tar.zst`.
**Accept:** `bun run --cwd packages/app build` emits `dist/index.html` +
`dist/assets/*`, non-map total ≈ 34 MB; `bun run pack` writes `ui.tar.zst`.

### M2 — Embed & serve (this repo)
New crate `crates/alforria-ui`: commits `assets/ui.tar.zst`, `build.rs` copies it
into `OUT_DIR` and the crate embeds it (`include_bytes!`), exposing an
`EmbeddedUiBackend: UiBackend` that decompresses the tarball once (zstd →
`HashMap<path, UiFile>`). Wire it into the production context
(`crates/alforria-server/src/lib.rs`) and the CLI `serve` path. Keep
`EmptyUiBackend` for tests.
**Accept:** `alforria serve` + `curl localhost:PORT/` returns `index.html` with
the `oc-theme-preload-script` CSP hash; unknown deep links return `index.html`;
`/site.webmanifest` and manifest PNGs bypass auth; unknown `/api/*` still 404s.

### M3 — Branding / theme
Change user-facing surfaces to `alforria` per `AGENTS.md`: `index.html` `<title>`,
wordmark/logo component (`@opencode-ai/ui/logo`), default theme tokens
(`packages/ui/src/theme/themes/*.json`), favicon/app icons, and app name
strings in `packages/app/src/i18n/en.ts`. Wire-compat surfaces (`opencode.json`,
`OPENCODE_*`, `opencode` theme asset id) stay.
**Accept:** served UI shows the `alforria` wordmark/title; no `OpenCode` in the
visible chrome; theme parity tests still green.

### M4 — Wire parity
Make the unmodified UI fully functional against the alforria server (event
streams, session pages, prompts). **Landed:** the app's v1 auto-detection
(`/global/health`) selects v1, both health endpoints and the whole v1 surface
answer correctly, `/global/event` SSE connects, and home + session pages render
live data. Next: UX iteration (presentation-only, wire-compatible).

### M5 — Multi-pane grid
A grid mode in the main content area tiles session and terminal panes side by
side, independent of the tab strip; panes are resizable, addable, closable, and
persisted per server. Panes are same-origin iframes onto chrome-free embed
routes (`?embed=1`), which suppresses titlebar/tab-store side effects and lets
each pane carry its own providers/event stream. Terminal panes use a dedicated
workspace-scoped PTY embed route.

**Landed:** nested horizontal/vertical splits (arbitrary tree), per-pane
maximise/restore, split-right/split-down controls, drag-to-reorder (swap), and
the first flat schema migrates into a row split on load. The tab strip is hidden
while grid mode is active so a tab click can never be masked by the grid.
**Battle-tested** in headless Chrome (23/23 checks): corrupt-store recovery,
unknown-session panes, 8-pane layouts, close-all → tabs, maximise-then-close,
rapid toggling, and deep links.

### M6 — Desktop-first grid and tabbed panes
Grid is the **default on desktop** (≥768px) and always disabled on mobile,
where the normal tab strip renders. Mode is stored as `"auto"` and resolved
against the viewport, so an explicit user choice still wins on desktop.

Each grid **pane is now a tab container**: it holds several session/terminal
tabs with its own tab strip, per-tab close, and an add-tab menu; splitting
creates a new pane. The previous single-view pane shape migrates into a
one-tab container on load. Verified in headless Chrome (15/15): desktop default
grid, add pane, add/switch/close tab, split, persistence, and zero exceptions.

Still to come in M6: a left tree of projects + sessions with drag-and-drop into
panes, and new-session placement (tab in the focused pane, or new space).

### M6 side tree and new-session placement (landed)
A left sidebar in grid mode lists every project the server knows and, per
project, its sessions (fetched per directory). Sessions can be **clicked** to
add a tab to the focused pane or **dragged** onto a pane to add a tab, onto the
empty grid area to create a new pane. Each project row has a **New session**
control whose menu creates a session and opens it either as a tab in the focused
pane or as a new grid space. Verified headless (23/23 overall): tree listing,
click-to-tab, drag-to-pane, drag-to-new-pane, and both new-session placements,
with zero exceptions.

### M6 grid UX redesign + cross-session navigation (landed)
Chrome overhaul driven by user feedback ("the whole grid and tabs system is
broken"): ButtonV2/IconButtonV2/MenuV2/TooltipV2 chrome, one add-menu per pane
header plus a toolbar add button, an empty-grid state with a New session CTA
and drop hint, drop-region indicators while dragging, and a drag cover so
iframes cannot swallow drag events. Pane titles are relayed from embedded
sessions via postMessage (never raw `ses_…` ids).

Cross-session links route through a new **open-session channel**:
- already open somewhere → focus that pane and activate its tab,
- a subagent session → open in the pane showing its parent,
- pane-local navigation (parent breadcrumb, archive fallthrough) → replaces the
  origin pane's active tab in place,
- anything else → opens in the focused pane.

Same-window callers (notifications) dispatch a cancelable window event and fall
back to route navigation when no grid handles it; embedded panes postMessage
up (iframes carry a `pane` id so requests know their origin). Notifications now
also fire for subagent sessions. Verified headless (17/17): empty-state CTA,
add menus, tab switch/close, sidebar drops (center + edge), tab move, reload
persistence, iframe pane params, open-session focus/dedupe, and close-all.

### M6 review pass — state retention, focus sync, a11y (landed)
A four-agent review (UX heuristics, accessibility, visual consistency,
correctness) drove a hardening pass:

- **State retention:** per-tab persistent iframes (tab switches and
  maximize/restore no longer reload panes), `collapse()` preserves resize
  proportions on tab close/move, `resizeNode` rejects stale drag arrays.
- **Focus sync:** embedded panes relay pane focus to the grid so toolbar and
  sidebar actions target the pane the user is actually in; focused pane gets a
  visible outline; active tab chip scrolls into view.
- **Lifecycle:** deleted sessions prune their grid tabs and title relay; the
  session cache is server-scoped and invalidated on deletion;
  `replaceActiveTab` never destroys terminal tabs and closes the origin tab
  when the target is already open; split with a vanished target no longer
  persists a dangling focus id.
- **A11y + consistency:** tab chips are real ARIA tabs (roving arrow keys,
  Enter/Delete, focus rings, keyboard-revealed close); splitters are focusable
  with arrow/Home/End resize and `aria-valuenow`; the grid toggle and split
  buttons use layout glyphs instead of ambiguous icons; the sidebar gained a
  hidden scrollbar, loading skeletons, and pressed/focus states; postMessage
  handlers validate origin. Unit suite grew to 31 grid tests; harness 17/17.

## Cross-track contracts

- **Dist is build output**, never committed; Rust builds must not *require* a
  prebuilt dist (embed empty and fall back to the 404 envelope if absent).
- **Wire compatibility wins**: no change to JSON shapes, `type` tagging, or
  optional-vs-null. Branding changes are presentation-only.
- Rebuild/embed is a documented step: `bun run build` in `web/`, then `cargo build`.
- `fixtures/` stays frozen.

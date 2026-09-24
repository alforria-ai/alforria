# Web UI: reuse upstream `packages/app`, embed it, improve it

Status: M1 (fork + build + pack) and M2 (embed + serve) **landed**. M3
(branding/theme) next.

- `alforria-ai/web` @ `2026165` holds the fork and `script/pack.ts`.
- `crates/alforria-ui/assets/ui.tar.zst` (8.7 MB, 954 files) is embedded and
  served via `EmbeddedUiBackend`; `OPENCODE_DISABLE_EMBEDDED_WEB_UI=1` reverts
  to the empty backend.

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

### M4+ — Wire parity & UX
Make the unmodified UI fully functional against the alforria server (event
streams, session pages, prompts), then iterate on UX. Scoped after M1–M3 land.

## Cross-track contracts

- **Dist is build output**, never committed; Rust builds must not *require* a
  prebuilt dist (embed empty and fall back to the 404 envelope if absent).
- **Wire compatibility wins**: no change to JSON shapes, `type` tagging, or
  optional-vs-null. Branding changes are presentation-only.
- Rebuild/embed is a documented step: `bun run build` in `web/`, then `cargo build`.
- `fixtures/` stays frozen.

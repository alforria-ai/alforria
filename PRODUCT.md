# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Stack

Rust workspace (see `AGENTS.md`). The web client is a new SolidJS + TypeScript + Vite app in `web/`, chosen on 2026-10-03 to replace the forked upstream UI (`alforria-ai/web`); its packed bundle is committed to `crates/alforria-ui/assets/ui.tar.zst` so Rust builds never need a JS toolchain. Web-first, desktop-ready: a thin platform layer (folder picker, OS notifications, open-in-editor) lets a Tauri shell wrap it later.

## Users

Developers who run AI coding agents as part of daily work and supervise many of them at once: typically a fleet of 7+ concurrent sessions (plus their subagents) across several repositories. They use the web client in every setting:

- from a laptop browser against `alforria serve` on a workstation over the LAN or SSH;
- on localhost, on the same machine;
- on a big monitor for long supervision sessions lasting hours;
- from a phone, to check status, approve a tool call or answer a question.

Their job is to keep the fleet moving. That means spotting which agent is blocked on them, unblocking it quickly, reviewing what agents changed, and diving deep into one or two sessions when needed.

Secondary audience: contributors and the LibertAI community.

## Product Purpose

alforria is an agentic coding tool. It pairs a repository with LLMs to read, plan, write, edit and review code, run tools, and manage sessions locally, from a single Rust binary. It provides a terminal UI, a local HTTP/SSE server, and command-line access to the session engine.

The web client is the multi-session supervision surface for that server. Success means a developer can run a dozen agents and never lose track of one that is waiting on them.

## Positioning

alforria is its own product, not "the Rust port of opencode". The opencode origin may be acknowledged in one line; it never carries headline billing.

The web client is built around supervising many agents at once, rather than chatting with one: an overview of every live session, with the ability to act on blocked sessions without opening them. It speaks the opencode-compatible wire format, so it is a client of the documented server API rather than a privileged internal surface.

## Operating Context

- **Server:** `alforria serve` (v1 HTTP API plus the `/global/event` SSE stream; PTY over WebSocket). HTTP Basic auth is opt-in via `OPENCODE_SERVER_PASSWORD`, with username default `alforria`.
- **How the UI ships:** embedded in the binary and served at `/`.
- **Scoping:** sessions are scoped to project directories, and one server serves many projects.
- **Rhythm of use:**
  - agents work for minutes to hours;
  - they pause for permission requests (tool calls matching `ask` rules, repeated-call doom-loop gates) and for questions (multiple-choice and free-text);
  - they spawn subagent sessions;
  - they produce file changes tracked by Git-backed snapshots that can be reverted.
- **Providers:** LibertAI models (OpenAI-compatible endpoint, `https://api.libertai.io/v1`) and other providers.
- **Embedding and cloud sync:** alforria is being embedded in `libertai-cli`. Standalone alforria's hierarchy is project → session; this web client targets that.
  - A separate LibertAI cloud product, built on top later, adds account → machines/servers: users sign in with their LibertAI account, see every server running on their machines, and connect to one.
  - The client must leave a clean slot for that machine level (an injectable server switcher/connection layer) without designing it now.
  - Still undecided: the relay/tunnel mechanism, where the cloud UI is hosted, and the auth flow.

## Capabilities and Constraints

- **Wire compatibility is binding.** Every JSON shape matches the opencode reference. The client uses v1 routes, where v2's prompt, history and revert are stubbed.
- **Message model:** user/assistant messages with typed parts:
  - text, reasoning and file;
  - tool, whose state is pending, running, completed or error;
  - step-start/finish carrying cost and tokens;
  - patch, snapshot, agent, subtask, retry and compaction.
- **Session status:** idle, busy or retry. Sessions also carry a todo list and diffs, and support fork, revert/unrevert, summarize (compaction), share and abort.
- **Event types the client consumes:**
  - `message.updated`, `message.part.updated`, `message.part.delta` (streaming);
  - `session.status`, `session.idle`, `session.error`, `session.diff`, `todo.updated`;
  - `permission.asked` (reply `once`/`always`/`reject`);
  - `question.asked`;
  - `pty.*`, `file.edited`, `vcs.branch.updated`;
  - `server.heartbeat` every 10s.
- **Production tool registry (14 entries):** invalid, question, bash, read, glob, grep, edit, write, task, webfetch, todowrite, websearch, skill, apply_patch. MCP tools may add more.
- **First-build scope** beyond chat:
  - review changes (per-session diff with revert);
  - terminals (PTY);
  - a file viewer;
  - settings (providers/auth, models, agents, MCP, permissions).
- **Not available yet:**
  - symbol search (`/find/symbol` returns `[]`);
  - console org switching;
  - `?workspace=` scoping without `OPENCODE_WORKSPACE_ID`.
- **Undecided:** published install channel; logo/mark asset beyond the wordmark.

## Brand Commitments

- **Name:** `alforria`, lowercase, on user-facing surfaces; released by LibertAI. Wire/config identifiers keep `opencode` (`OPENCODE_*` env vars, `opencode.json`, `x-opencode-*` headers).
- **Visual identity:** the web client derives from the alforria "component datasheet" identity recorded in `~/repos/alforria-website/DESIGN.md`, adapted to a dense, dark-first operating tool with a light theme. This was chosen by the user on 2026-10-03.
- **Fresh design:** LibertAI Desktop (`~/repos/libertai-code-desktop`) is a capability checklist only, never a visual or layout template. The user asked for the design to be rethought from first principles.
- **Claims:** no invented testimonials, benchmark numbers or adoption claims.

## Evidence on Hand

- **Source and API references:**
  - API surface: `crates/alforria-server/src/routes/v1/`, `crates/alforria-server/src/sse.rs`;
  - schemas: `crates/alforria-schema/src/session_v1.rs`, `event_manifest.rs`;
  - reference client: the TUI (`crates/alforria-tui/src/transport/`).
- **Brand assets:** website tokens, Barlow Condensed WOFF2 (OFL) and illustrations in `~/repos/alforria-website/public/`. Real TUI captures in the same repo.
- **Data:** no real session data is bundled. Prototypes must use clearly synthetic sessions.

## Product Principles

- **The blocked agent comes first.** Anything waiting on the human outranks everything that is merely running.
- **Act where you see it.** Approve, answer, abort and review from the place the information appears, without navigating away.
- **One source of truth.** One event stream, one store. A permission or question exists once, no matter how many places show it.
- **Scale without squinting.** Layouts must stay legible at a dozen-plus sessions and still work on a phone.
- **Verifiable, not decorative.** Show real state (tokens, cost, diffs, tool output) precisely; never fake progress or embellish numbers.

## Accessibility & Inclusion

- Target WCAG AA contrast in both themes.
- Full keyboard operation: approvals, questions and navigation.
- Visible focus and reduced-motion support.
- Readable on small screens.
- Status must never rely on colour alone.

# Changelog

All notable changes to alforria are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). alforria is
pre-1.0, so minor releases may include breaking changes.

## [Unreleased]

### Added

- Dynamic workflows: saved JavaScript orchestrations that fan work out to many
  agents, a `workflow` tool, and progress views in the web client and the TUI
  (`ctrl+x w` or `/workflows`).
- A `subagents` tool, so an agent can list, inspect, wait on and cancel the
  subagents it started.
- The TUI and the web client show working subagents and the current todo above
  the prompt, and show resuming, recovered and failed states on task rows.
- `alforria db trim-diffs [--vacuum]` shrinks a database that already holds
  oversized diffs.

### Fixed

- A turn whose snapshots include a large unignored directory, such as
  `node_modules`, no longer costs tens of GB of memory and disk. Each turn keeps
  at most 4 MB of patches, with line counts for every file, the event log no
  longer stores patches, and agent steps no longer reload them.

### Changed

- A subagent that stops on a recoverable provider error resumes automatically,
  up to twice. If it still fails, its parent gets the partial output and how to
  continue it.
- A subagent the user prompts directly reports its result to its parent, which
  starts a turn on it.
- Quitting the TUI while a workflow runs asks for a second press.

### Fixed

- A provider answering with an empty response no longer makes a session call it
  in a tight loop forever: the turn fails after three such responses.
- A provider that goes silent is timed out after `chunkTimeout` (default 5
  minutes) and retried, instead of holding the session indefinitely.
- Pasted line breaks no longer submit the prompt on terminals without bracketed
  paste.
- A prompt naming no agent no longer switches a subagent to the default agent.
- TUI: the `view subagents` hint names its key, and up in a child session no
  longer recalls the prompt history.

## [0.3.0] - 2026-10-27

### Added

- A DeepSeek prompt family in the core prompt catalog.
- Read-before-write guards for file edits.

### Fixed

- The DeepSeek image placeholder is neutralized for DeepSeek models instead of
  being sent to the provider as content.
- An oversized buffered server response is no longer emptied before it is
  returned to the client.
- Abandoned permission and question requests are cleared from clients, and
  replies resolve across loaded instances.

## [0.2.1] - 2026-10-16

### Fixed

- The session engine survives stream faults and length cuts from providers
  instead of stopping the turn.

## [0.2.0] - 2026-10-13

The embedded web UI, a hardened server, and a TUI parity push.

### Added

- A rebuilt SolidJS web client embedded in the binary: multi-pane desktop
  grid, session queue, worktrees, review comments on diffs, fork/export/step
  through messages, prompt history and surviving drafts, `!` shell commands,
  a reasoning-variant picker, model/MCP/LSP status per project directory,
  archived sessions, and a first-run connect-a-provider flow.
- Keyboard operation across the web client, 40px phone targets, an AA light
  theme, and an accessibility pass with zero axe-core violations.
- Web end-to-end and fleet-scale test suites running against the real server.
- `serve` now generates a password by default and prints the credentials.
- Origin checks for PTY websockets and cross-site writes, plus CORS control.
- TUI markdown reply parity, prompt-bar parity, dialogs that own the keyboard,
  custom themes from `themes/*.json`, a system theme from the real terminal
  palette, and `tui.json` configuration.
- TUI session-list search through the server, `@` mentions for files and MCP
  resources with line ranges, mouse text selection, and OSC 52 copy for SSH.
- A browser-based `Sign in with LibertAI` flow in the TUI.

### Changed

- Plain `alforria` runs its server in process with no TCP listener. Only
  `alforria serve` and `alforria web` bind a port.
- Provider failures map to the AI SDK `APIError` shape and retry like the
  upstream implementation, including transport failures.
- Built-in LibertAI models no longer carry a fabricated release date.

### Fixed

- Windows: git is found again as `git.exe`, restoring project resolution,
  worktrees, and diffs.
- Windows: directory strings are normalized on both publish and subscribe
  paths, so SSE subscribers receive session events and `run`/`attach` no
  longer hang.
- Windows: TUI signal handling and the shell kill ladder are correctly gated.
- `file.edited` is published, step-finish waits for forked tools, and
  credentials reach already-open instances.
- Tests are hermetic across XDG paths and canonicalized directories, and are
  less flaky.

## [0.1.0] - 2026-09-22

First release.

### Added

- An opencode-compatible CLI: `run`, `acp`, `serve`, session commands, and
  the terminal UI.
- A built-in `libertai` provider with a live model catalog.
- Browser SSO login for LibertAI (`alforria auth login -p libertai`) and
  `alforria libertai usage`.
- Linux x86_64 and Windows x86_64 release binaries.
- A universal installer for Linux, macOS, and WSL.
- The Rust workspace: schema, LLM, core, server, TUI, embedded UI, and CLI
  crates.

[Unreleased]: https://github.com/alforria-ai/alforria/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/alforria-ai/alforria/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/alforria-ai/alforria/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/alforria-ai/alforria/releases/tag/v0.1.0

# Changelog

All notable changes to alforria are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). alforria is
pre-1.0, so minor releases may include breaking changes.

## [Unreleased]

## [0.4.2] - 2026-11-12

### Security

Found by a review of v0.4.1.

- A symlink inside the project that points out of it was read, written,
  searched and patched without the `external_directory` prompt.
- The shell tool's `external_directory` check missed most commands: it only
  looked at a short list (`cat`, `rm`, `cp`, …), dropped `$HOME` and `~user`
  arguments, ignored `--opt=path`, and did not follow `cd`.
- `bash` permission rules were matched against the command's source text, so
  `X=1 rm`, `/bin/rm`, quoting, `sh -c`, `eval`, `env`, `sudo`, `command`,
  `xargs` and `find -exec` all bypassed a rule for `rm *`.
- The `task` tool accepted any session id as `task_id`, let the model pick a
  primary agent, and did not pass the parent agent's deny rules to the child,
  so a plan session could start a `build` subagent and edit. A prompt written
  by the model attached any `@`-mentioned file without a permission check.
- `DELETE /experimental/worktree` deleted any directory it was given.
- Undoing a turn could delete files outside the worktree named by an imported
  session.
- `alforria web`, `alforria acp` and a TUI started with `--port`/`--hostname`
  ran an unauthenticated server when `OPENCODE_SERVER_PASSWORD` was unset.
  They now generate a password, as `serve` does; `web` prints it.
- `/api/fs/read` served project `.svg` and `.html` files as documents in the
  server's origin; they are now sandboxed.
- `GET /file` listed directories outside the location.
- The TUI printed the session title after exit without removing control
  characters.
- Provider requests followed redirects to other hosts with `x-api-key` and
  `x-goog-api-key` attached.

### Fixed

- Web client: the `/` command menu could reopen after a message was sent and
  cover the goal row; the providers list no longer wraps its status and
  button onto two lines.

## [0.4.1] - 2026-11-12

0.4.0 was tagged but shipped no binaries: its macOS verification caught the
bug below.

### Fixed

- macOS: a `/goal` left active by a server that has since exited now comes
  back paused. Its owner was always taken to be alive, so the goal stayed
  "active" with nothing driving it.

## [0.4.0] - 2026-11-12

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
- Session history in the web client (`H`, the History buttons, or ⌘K): every
  session the server keeps, however old, filtered by folder and title. This
  includes folders that have dropped off the overview. An opened session
  rejoins the session list, and stays once it is used again.
- The web client's session lists fold a parent's subagents away: the toggle
  beside its title, `←`/`→` on a row, or "Hide all subagents" in ⌘K. Folded,
  the toggle says how many subagents are working or waiting. Nested subagents
  indent one step per level.

- `/goal <objective>` keeps a session working until a reviewer agrees the
  objective is met. After each turn the model judges the conversation against
  the objective, with no tools, and either finishes the goal or sends the
  session on with its notes. It stops when the user stops the session, when a
  turn fails, when the reviewer says the user is needed, after 3 rounds
  without progress, after 50 rounds, or when the server restarts. `/goal
  pause`, `resume` and `clear` control it. The TUI sidebar and the web client
  show the goal; `GET /session/{id}/goal` and the `goal.changed` event expose
  it.
- A forked session starts with a note telling the model it is a fork, so it
  does not wait on or redo background work that belongs to the original.
- `alforria migrate` moves an install from the `opencode` directories to
  alforria's own (see Changed). `--copy` copies the data instead of moving it,
  `--fresh` starts a new database and keeps the saved logins, `--dry-run`
  previews.
- Remote MCP servers stay signed in: OAuth tokens refresh before they expire
  and on a 401.
- ACP clients are told when a turn is waiting on a provider retry.

### Fixed

- A file path containing `..` could write, search or read outside the project
  without the `external_directory` prompt, through `apply_patch`, `grep`,
  `glob` and `lsp`.
- `webfetch` followed redirects without checking them against the `webfetch`
  permission, accepted credentials in the URL, and buffered a response of any
  size before applying the 5 MB limit.
- Shell redirections (`> file`) now count for `external_directory`, and a
  command made only of a redirection still needs the `bash` permission.
- A patch that moved a file onto itself deleted it.
- Undoing a turn that renamed a file restores the original path.
- A custom command no longer re-expands `$1` or `$ARGUMENTS` typed as an
  argument, and `$0` stays literal.
- A session title can no longer inject terminal escape sequences through the
  TUI's window title.
- MCP logins were forgotten on every restart, and saving one login erased the
  others.
- `~` expands in a local MCP server's `cwd`.
- OpenAI-compatible streams: an error sent inside the stream is reported as
  what it is (rate limit, context overflow, authentication) instead of an
  undecodable event; keepalive frames, frames without `choices` and tool-call
  deltas without an `index` are accepted; a tool call cut off by the output
  limit no longer fails the turn; cached tokens are counted wherever the
  provider reports them.
- Context-overflow detection recognises LiteLLM, DeepInfra, Novita, Z.ai,
  Nova and Workers AI wording, and no longer mistakes an output-limit error or
  Bedrock throttling for overflow.
- Anthropic: a usage-only `message_delta` no longer ends the message twice,
  and thinking from another model is not replayed without a signature.
- Gemini: tool-use prompt tokens count as input.
- ACP: an approved edit is no longer written twice; the command list arrives
  after the response that announces the session; a permission answered in
  another client is settled; a session opened and closed without a prompt is
  deleted.
- Web client: text that streamed in during an overlapping reload is no longer
  truncated; the Changes pane no longer blanks when a turn ends; the
  transcript keeps its reading position across Changes and Files; queued
  follow-ups wait for the previous prompt to start; notifications from
  same-titled sessions no longer replace each other.
- A turn whose snapshots include a large unignored directory, such as
  `node_modules`, no longer costs tens of GB of memory and disk. Each turn keeps
  at most 4 MB of patches, with line counts for every file, the event log no
  longer stores patches, and agent steps no longer reload them.
- A provider answering with an empty response no longer makes a session call it
  in a tight loop forever: the turn fails after three such responses.
- A provider that goes silent is timed out after `chunkTimeout` (default 5
  minutes) and retried, instead of holding the session indefinitely.
- Pasted line breaks no longer submit the prompt on terminals without bracketed
  paste.
- A prompt naming no agent no longer switches a subagent to the default agent.
- TUI: the `view subagents` hint names its key, and up in a child session no
  longer recalls the prompt history.
- Signing in or out can no longer erase the other providers' saved keys: a
  credential file that cannot be read or parsed is left alone, and it is
  replaced in one step. Every credential change is recorded, without the key,
  in `auth-audit.log` beside `auth.json`.
- Running the test suite no longer rewrites the developer's own `auth.json`.
- Web client: a provider connected through config or the environment offers
  Sign in (or Set key), so a missing key can be restored from the browser.

### Changed

- alforria keeps its global config, data, cache and state in `alforria`
  directories (`~/.config/alforria`, `~/.local/share/alforria`, …) instead of
  sharing upstream's `opencode` ones. opencode v2 migrates a shared database
  to a schema alforria cannot read. An existing `opencode` directory is still
  used until its `alforria` counterpart exists, so nothing changes for a
  current install until `alforria migrate` is run. A database already migrated
  by opencode v2 is refused with an explanation. Project-level `opencode.json`
  and `.opencode/` are read as before.
- A subagent that stops on a recoverable provider error resumes automatically,
  up to twice. If it still fails, its parent gets the partial output and how to
  continue it.
- A subagent the user prompts directly reports its result to its parent, which
  starts a turn on it.
- Quitting the TUI while a workflow runs asks for a second press.
- The web client's overview keeps sessions for 7 days instead of 3.

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

[Unreleased]: https://github.com/alforria-ai/alforria/compare/v0.4.2...HEAD
[0.4.2]: https://github.com/alforria-ai/alforria/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/alforria-ai/alforria/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/alforria-ai/alforria/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/alforria-ai/alforria/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/alforria-ai/alforria/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/alforria-ai/alforria/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/alforria-ai/alforria/releases/tag/v0.1.0

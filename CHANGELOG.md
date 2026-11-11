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
- `alforria serve` and `alforria web` shut down cleanly on SIGTERM and SIGINT:
  they stop accepting, cancel the running turns (or let them finish, with
  `server.shutdown.sessions: "drain"`), kill the terminals and tool processes,
  checkpoint the database and exit 0, all within `server.shutdown.timeout`
  (default 10 s). See `docs/unattended-server.md`.
- Bearer authentication next to HTTP Basic (`server.auth.bearer` in the
  global config): static tokens stored as SHA-256 digests, and Ed25519 JWTs
  verified against configured public keys, with `exp`, `nbf`, `aud` and an
  optional `iss` enforced. A token carries a scope: `chat` (sessions,
  messages, events, permission and question replies, project files),
  `connector` (chat plus local automation) or `admin` (everything). Routes
  not listed for a scope need `admin`. `server.directories` lists the project
  directories (matched exactly) a token narrower than `admin` may open; by
  default that is the server's working directory. With bearer authentication configured and no
  password set, `alforria serve` generates none. A token below `admin` cannot
  run shell directly or a command with inline shell, set permission rules,
  switch tools on, pick hidden agents, attach files from outside the project
  or answer a permission request `always`
  (`server.auth.bearer.reply_always` opts in). These limits are enforced in
  the engine on the values it resolves, not on the request as written, and
  they stay with the session: one created by such a token is restricted for
  life, with its subagent sessions and forks, so goal rounds and other turns
  the engine starts are bound too. Such a token acts only in restricted
  sessions: an operator's session it may read or fork (the fork is
  restricted), nothing else. `DELETE /session/{id}/restriction` (`admin`)
  lifts a restriction. The threat model is in `docs/unattended-server.md`.
- Files for browser clients: `POST /file/ticket` mints a short-lived read
  ticket for one project file, usable as `?ticket=` on the new
  `GET /file/raw` (the file's bytes, with ranges, served sandboxed) and on
  `GET /file/content`; `PUT /file/content?path=` uploads a file into the
  project, up to `server.upload.max_bytes` (default 32 MiB). Paths are
  confined to the project, symlinks and `..` included; a token below `admin`
  uploads only into `server.upload.directories` (default `uploads/`) and
  never to a config, instruction, agent, command, skill, workflow or git
  file.
- `permission.unattended` and `question.unattended` (global config) bound how
  long a permission ask or a question waits for a person: after `timeout`, an
  ask is denied or allowed once and a question is dismissed, the model is
  told nobody was there, and the turn continues. The default, `wait`, is the
  old behaviour. An automatic answer is marked `automatic: true` on
  `permission.replied` and `question.rejected`.
- `GET /attention` lists the permission asks and questions currently waiting
  for a person, across projects, with their session, title and age.
- `hook.webhook` (global config) delivers selected events
  (`permission.asked`, `question.asked`, `session.idle`, `session.error`, or
  any other by name or pattern) to a URL through a durable outbox: at least
  once, with exponential backoff, a delivery id to deduplicate on, an
  HMAC-SHA256 or Ed25519 signature over timestamp and body, and bounded
  retention. Deliveries survive a restart. The payload carries ids, the
  session and its title, never message content.
- `shell_env` (global config) controls what the agent's shells inherit from
  the server's environment: `inherit` (`"all"`, `"none"` or a list), `allow`,
  `set` and `deny`.

### Fixed

- Routes naming a session and a message, part or permission request inside
  it acted on the inner id without checking it belonged to that session:
  `POST /session/{id}/permissions/{permissionID}` could answer another
  session's ask, and the message, part and revert routes accepted another
  session's message id. The inner object must now belong to the session in
  the path, or the answer is 404.
- A custom command ran inline shell found in its *arguments*: `/cmd` with an
  argument containing `` !`…` ``, or one that broke out of the template's own
  `` !`…` ``, executed it with no permission check. Inline shell is now taken
  from the configured template only, and the arguments reach it as shell
  parameters (`$1`, `$ARGUMENTS`) instead of being pasted into the command.
  Templates that relied on an argument being pasted in as shell source
  (rather than as a value) need rewriting.
- Stopping a session `shell` command (`POST /session/{id}/shell`) did not
  kill it: the command was never given the process group the stop signals.
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

### Changed

- The agent's shells no longer inherit the server's secrets. The `bash` tool,
  the session `shell` command, command-template `` !`cmd` `` substitutions,
  terminals, worktree start scripts, MCP stdio servers and language servers
  start without the server password and auth variables and without provider
  API keys (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `LIBERTAI_API_KEY`, …; the
  list is in `docs/unattended-server.md`). To pass one through, name it in
  `shell_env.allow`, or in an MCP server's own `environment`.
- A global config file that does not parse now stops `alforria serve` and
  `alforria web` from listening, instead of serving with the defaults: the
  host config carries the server's access policy.
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

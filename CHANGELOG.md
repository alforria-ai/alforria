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
- Triggers: turns that start on a schedule or an event instead of a person.
  A `trigger` map in the config (or `POST /trigger`) pairs a prompt with a
  5-field cron expression in a real timezone, an interval, a one-shot time, a
  watched path, or a webhook. Each run is an ordinary session under the
  trigger's own permission rules, with the prompt marked
  `metadata.trigger = { id, run }`. A tick that finds the last run still going
  is skipped and recorded, missed ticks are not replayed (`catch_up` runs the
  latest one), and `quiet_token` lets a check that found nothing stay silent.
  Runs are announced by `trigger.fired`, `trigger.completed`, `trigger.failed`
  and `trigger.skipped`. Triggers are read from your own config, never from a
  project's, and a webhook run cannot act unless its trigger allows it. See
  `docs/assistant.md`.
- A `schedule` tool, so an agent can create, list, change, pause and delete
  its own triggers and one-shot reminders, gated by a `schedule` permission.
  It only changes triggers its own conversation scheduled, and what it
  schedules runs under the rules its creator had.
- Long-term memory (`memory.enabled`, off by default): a `memory` tool that
  saves typed records (fact, preference, task, note) and searches them, the
  most recent of which are added to the system context of every step in a
  fenced `<memory>` block, and a `session_search` tool for full-text search
  across past sessions. `memory.scope` keeps memory per project or shares it;
  `memory.flush_before_compaction` gives the model one bounded chance to save
  what matters before a compaction. Also `GET/POST /memory`,
  `GET/PATCH/DELETE /memory/{id}`, `GET /memory/search` and
  `GET /memory/session`. The transcript index is built in the background and
  costs nothing when a message is written. Records carry where they came
  from, and those written by a flush or by a run that carried outside text
  are searched but not injected. A session counts as having seen outside
  content once a web, MCP or plugin tool answers in it, or it reads a file
  from the upload area (`grep` and `glob`: when a file they returned is one
  of the uploads, not because an `uploads/` directory exists); such a
  session may change only unverified records it saved itself. No tool's
  permission depends on that mark, and with memory and triggers off (the
  default) nothing reads it. `memory` and `trigger` settings are read from
  your own config only; a project may at most switch memory off. See
  `docs/assistant.md`.
- `alforria import --format openai-chat <file|->` imports a chat transcript
  from another tool: a JSON or JSONL list of `{role, content, tool_calls,
  tool_call_id, created_at}` messages. Tool calls become tool parts with
  their results, images become file parts. `--live-tail <n>` or
  `--archive-before <n|time>` keeps older messages as readable, searchable
  history behind a compaction boundary carrying the transcript's `summary`,
  so the next prompt sends only the summary and the live messages.
  `--dry-run` reports what would be imported, and an `import_id` makes the
  import idempotent.
- Persona controls for products that embed alforria: `prompt.environment`
  (`true`, `false` or a template) for the `<env>` block,
  `prompt.model_identity` and `prompt.empty_skills` for the other fixed
  additions, and `tool.<name>.description` / `description_append` to reword
  a built-in tool.
- `provider.<id>.models.<name>.family` picks the default system prompt for a
  model whose id does not say what it is (a family name or the underlying
  model id).
- The `websearch` tool can use LibertAI search (on by itself when the
  `libertai` provider has a key) or any endpoint with the same contract
  (`websearch: { provider: "http", url, headers }`), and is then offered to
  every model.
- The built-in `libertai` provider accepts its catalog's aliases
  (`claw-flash`, `claw-core`, `deep-claw`, …) as model ids, with the target
  model's limits, cost, capabilities and prompt, and lists
  `deepseek-v4.1-flash-1017` and its `-thinking` variant.
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
  confined to the project where the kernel would land them: symlinks are
  followed where they stand and `..` is applied to where the path has got
  to (`link/../x` is beside where `link` leads), and a path with no such
  reading — a `..` after a directory that does not exist — is refused. The
  directory allowlist, the upload area, file tickets, a `chat` token's reads
  on the frozen `GET /file` and `GET /file/content`, prompt attachments and
  stored watch paths all use that one resolver. A token below `admin`
  uploads only into `server.upload.directories` (default `uploads/`) and
  never to a config, instruction, agent, command, skill, workflow or git
  file, nor to a file a formatter, language server or build tool is
  configured or started from (`package.json`, `eslint.config.*`,
  `Cargo.toml`, `node_modules/`, … — by name, any extension, defence in
  depth). The upload directory must be a directory of its own: `"."`, a
  path out of the project or a loader or tool directory in
  `server.upload.directories` is a config error and the server does not
  listen. Such a token's uploads are also held to a quota per upload
  directory — `server.upload.max_total_bytes` (default 1 GiB) and
  `server.upload.max_files` (default 1000 entries, directories included),
  at most 8 levels deep — answered `413` `UploadQuotaError` before
  anything is written, from a running count that is re-read at most every
  5 minutes. The write guard also protects the files a `formatter` or
  `lsp` entry's `command` names, and while bearer tokens are configured no
  language server is started on a file in an upload directory (nor would a
  formatter be run on one), so nothing uploaded is read as a tool's config.
- `permission.unattended` and `question.unattended` (global config) bound how
  long a permission ask or a question waits for a person: after `timeout`, an
  ask is denied or allowed once and a question is dismissed, the model is
  told nobody was there, and the turn continues. The default, `wait`, is the
  old behaviour. An automatic answer is marked `automatic: true` on
  `permission.replied` and `question.rejected`.
- `GET /attention` lists the permission asks and questions currently waiting
  for a person, across projects, with their session, title and age. A token
  below `admin` is shown the ones it could answer: in the directories it may
  open, in the sessions it may act in, and not an operator's session's.
- `hook.webhook` (global config) delivers selected events
  (`permission.asked`, `question.asked`, `session.idle`, `session.error`, or
  any other by name or pattern) to a URL through a durable outbox: at least
  once, with exponential backoff, a delivery id to deduplicate on, an
  HMAC-SHA256 or Ed25519 signature over timestamp and body, and bounded
  retention. Deliveries survive a restart, and a kill: the row is written
  before the event's publish returns. The payload carries ids, the session
  and its title, never message content. Each hook is a queue of its own,
  delivered in order and at the same time as the others, so a receiver that
  hangs delays only its own deliveries. A 4xx other than 408, 425 and 429
  is given up at once instead of retried until `max_age`. Events deleted
  beyond `hook.outbox.max_pending` are logged and counted, and on a server
  with hooks `GET /attention` carries `outbox: { pending, failed, dropped }`
  for the operator (no authentication, Basic, or an `admin` token; never
  for `chat` or `connector`).
  Stopping the server does not wait for a receiver. Of several servers on
  one data directory, one delivers (`<data>/lock/outbox.lock`).
- `shell_env` (global config) controls what the agent's shells and helper
  processes inherit from the server's environment: `inherit` (`"all"`,
  `"none"` or a list), `allow`, `set` and `deny` (matched without regard to
  case). A server that is unattended or reachable from another machine —
  bearer auth, a non-loopback bind, `permission.unattended` /
  `question.unattended`, `hook.webhook`, a `webhook` or `watch` trigger, or
  `server.trust.project: "restricted"` — runs them in a restricted posture:
  an allowlist (`PATH`, `HOME`, `LANG`, `LC_*`, `TERM`, `TZ`, `USER`,
  `LOGNAME`, `SHELL`, `TMPDIR`, plus `allow`) with credential-looking names
  (`*_TOKEN`, `*_KEY`, `*_SECRET`, `AWS_*`, …) denied, for shells, MCP stdio
  servers, language servers and git alike. The posture is logged at startup,
  tightens (never relaxes) when such a trigger or listener appears while the
  server runs, and is shown by `GET /server/posture` (admin). A run that a
  webhook or watch trigger started always gets the restricted environment.
- `compaction.default_context`: the context window, in tokens, assumed for a
  model that declares none (128,000 when unset).
- A restart repairs the sessions the last process died in the middle of.
  Every running turn leaves a marker on disk; at startup, before the first
  request, a turn whose process is gone has its running tool calls closed as
  errors ("interrupted by a server restart"), its assistant message
  completed and its status set idle, so the history is valid for the next
  prompt and no client sees a session stuck busy. A `session.interrupted`
  event is published for each.
- `server.resume` says what happens next: `"none"` (the default) leaves it
  there, `"notify"` adds a note the model reads with the next prompt, and
  `"continue"` prompts the model to carry on. A turn is resumed once: if the
  server dies again while resuming, the turn is left with a note. Subagents
  and `!shell` commands are repaired but never resumed, and `/goal` still
  comes back paused.
- `storage.synchronous`: `"full"` puts every commit on disk before it
  returns, for machines that are stopped by being killed; `"normal"`, the
  default and upstream's setting, never corrupts the database but can lose
  the last few seconds on a power cut.
- `storage.retention.events` bounds the event log, which until now only
  shrank when a session was deleted: `max_age` (`"30d"`, `"12h"`, …) drops
  older events, `keep_last` keeps each session's newest N whatever their age
  (on its own, only those). The server applies it at startup and daily.
  Unset, nothing is dropped. Messages and sessions are never touched.
- `alforria db prune [--max-age <duration>] [--keep-last <n>] [--vacuum]`
  applies that policy on demand.
- `GET /api/session/{id}/event` sends an `event.reset` frame wherever the
  events its reader was due next have been pruned, so a client reloads the
  session instead of silently missing them: first when the events after
  `after` are gone, and in the middle of a replay when retention drops
  events the reader had not reached yet. The frame comes before the first
  event after the hole. Without retention it never appears.
- The server folds the write-ahead log back into the database every five
  minutes, so the `-wal` file no longer sits at its high-water mark.
- Scoped tokens reach the assistant features. A `chat` or `connector` token
  manages its own schedules (`/trigger*`) under the `schedule` tool's rules —
  it never sets a trigger's `permission`, `agent` or `model`, names only
  sessions it may act in, and changes or fires only triggers a restricted
  caller created — reads and writes memory (`/memory*`) under the `memory`
  permission, and may replay a session's durable events
  (`GET /api/session/{id}/event`). A new `hook` scope opens
  `POST /trigger/{id}/hook` and nothing else, for whatever calls a webhook
  from outside; `connector` and `admin` include it, `chat` does not.
- A trigger records whether a restricted caller created it (`restricted` on
  the record). Its runs happen in restricted sessions only, and every
  engine-started turn has a defined caller: see "Turns nobody requests" in
  `docs/unattended-server.md`.
- `hook.webhook[].trigger_text: true` adds a trigger run's final answer to
  its `trigger.completed` delivery. Without it a delivery carries ids only,
  as before; the trigger, restart, media and memory events are all
  selectable by name or pattern.
- One table (`EXTENSION_KEYS`) says which config may set each alforria key:
  host, user or project. See "Which config may set what" in
  `docs/assistant.md`.

### Fixed

- A session is created together with its restriction, marks and ceilings,
  in the transaction that inserts it: a crash or a concurrent read between
  the two no longer finds a restricted caller's session unrestricted.
- Two more places judged a session by its agent's policy alone, without the
  session's own rules and ceilings (a trigger creator's bounds, the
  outside-input ceiling): the v2 permission ask
  (`POST /api/session/{id}/permission`), and the `/memory` routes for a
  token below `admin`. The v2 ask is now answered by the session's policy,
  as every turn is; a memory write that names a session (`?session=<id>`)
  is judged by that session's policy, and one that names none by the
  default agent's as before. The goal reviewer's request, which has no
  tools, now carries the session's policy too. For a session with no rules
  or ceilings of its own, nothing changes.
- A token below `admin` could write to an operator's session by
  percent-encoding the session id in the path (`/session/ses%5F…` with a
  `?directory=` it may open): the check that keeps it out looked the
  session up under the raw path segment, found none and refused nothing,
  and the handler then decoded the id — rename, metadata, archive, delete,
  revert, abort and the rest. The same spelling read a session of a
  directory outside `server.directories`. The session in a path is now
  resolved as the router decodes it, and the write check is one guard over
  every route that names a session, applied after routing; only `fork` is
  exempt, by name. `GET /permission` and `GET /question` list such a token
  the asks it may answer, as `GET /attention` does, in the same shape.
- The `<memory>` block shared its record slots between writer classes
  (the operator's records and a restricted writer's, verified and not) but
  spent `memory.inject.max_chars` in plain recency order, so a few long
  recent notes from one class used the whole character budget and the
  other class's records, though chosen, were never printed. The character
  budget is now shared the same way: an equal share per class present,
  newest first within it, and what a class leaves unused goes to the
  others. A session that is shown one class — every session without bearer
  tokens or `inject.unverified` — gets the block it got before.
- A background subagent's result reached its parent session as a message
  without the subagent's outside-content mark: a subagent that fetched a
  page in the background handed its findings to a parent that still
  counted as having seen nothing from outside, so memory the parent then
  wrote was verified. The parent is now marked before the result is
  delivered, as it is when the `task` tool returns the same result. The
  same holds for a subagent the user continued directly. The mark is
  provenance: no permission changes.
- A trigger could be pointed at an existing session in ways that left its
  run without the bounds it needed: one the `schedule` tool made, moved to
  another conversation by an update, ran there without its creator's
  bounds (they are stored only on sessions a trigger makes), and a run
  that arrived with a payload in a named session got neither the read-only
  ceiling nor the outside-start mark. The bounds of a run are now always
  carried by the session it runs in, so which session a trigger may name
  is restricted instead: a `webhook` or `watch` trigger, or a run with a
  payload, never names one; a trigger the `schedule` tool made names only
  the conversation that scheduled it (`current`, for `cron`, `interval`,
  `at` and `in`), where the run is a turn under that conversation's own
  policy; one written after outside content was seen names only a session
  that has the mark; one you wrote names any session of the directory; a
  `chat` or `connector` token's, a restricted one. Checked at create, at
  update and at every run — a stored trigger that no longer fits fails its
  run with the reason. See "A trigger that names a session" in
  `docs/unattended-server.md`.
- A trigger run that was queued behind another when the server stopped no
  longer starts its turn after the shutdown has settled the others: it is
  recorded as skipped and, for a scheduled tick, the tick is given back. A
  turn that starts while a shutdown is settling the rest is settled too.
- A server killed between taking a scheduled tick and starting its run no
  longer loses the tick silently (a one-shot reminder never fired, even
  with `catch_up`). The run is stored in the same transaction as the tick,
  and the next start closes it as interrupted and gives the tick back.
- Two servers on one data directory no longer do the same work twice.
  A turn interrupted by a kill was resumed by every server that started
  afterwards (the marker was a liveness check, not a claim); a starting
  server closed the unfinished trigger runs of a server that was still
  running them; and a webhook call, a watched change or a manual run that
  reached both ran on both. Restart markers are now claimed with an atomic
  rename, trigger runs record their owner process and only an orphan's are
  closed, every fire is claimed in the database, and one process per
  directory schedules. See "Two servers on one data directory" in
  `docs/unattended-server.md`.

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
- A model that stayed unavailable (a 503 on every request) was asked three
  times for each of the retry policy's attempts, because the HTTP client
  retried twice by itself underneath it: eighteen requests where the policy
  says one attempt and five retries. A turn's requests are now retried by
  the policy alone, and the turn ends with the provider's error after the
  fifth retry. Title generation keeps its own two HTTP retries.
- An image sent to a model that takes no image input was dropped by the
  provider without a word, and the model answered that nothing was attached.
  The request now carries `[image omitted: the current model cannot read
  images]` in its place, the message gets a note saying so (shown to the
  person, never sent to the model), and a `session.media.omitted` event is
  published. The stored message keeps its image for a model that can read
  it. A custom model reads images when its config lists the `image` input
  modality or sets `attachment: true`; one that declares neither is treated
  as text-only, so a custom vision model must now say so.
- Whole numbers in provider requests (`max_tokens`, `temperature`,
  `budget_tokens`, …) are written as integers, `4096` rather than `4096.0`,
  which some OpenAI-compatible servers refuse.
- A goal armed before a reboot could stay "active" with nothing driving it
  when the new server got the process id the old one had.
- A session whose history no provider had counted (imported, or long and
  never compacted) was sent whole on the next prompt, however large. The
  request is now sized before it is sent, from the provider's own token
  counts where there are any and from the size of the text where there are
  none, and the session compacts first when it would not fit. A model with no
  declared context limit is held to a default ceiling instead of none.
- A request that still does not fit after three compactions in a row ends the
  turn with a `ContextOverflowError` instead of compacting forever.
- Compaction sent the whole history to be summarised in one request, so a
  history larger than the context window could never be compacted and every
  later prompt failed with "Conversation history too large to compact". The
  history is now summarised in pieces that each fit, every piece building on
  the summary of the ones before it, and is read from the database a page at
  a time: a turn holds at most 8 MiB of history in memory, whatever the
  session's size. A session already stuck in that state compacts on its next
  prompt. When a provider refuses a summary request as too large (its real
  limit is below the declared one), the pieces are halved and tried again.
- Every step of a turn re-read the session's whole history from the
  database, several times over, however much of it a compaction had already
  put behind a summary. A step now reads back to the last compaction and no
  further, so its cost follows the conversation since then, not the session's
  age. The per-turn diff summary, pruning, goal review and subagent reports
  got the same treatment, and a turn's tools share one copy of the history
  instead of holding one each.
- A reader of `GET /api/session/{id}/event` that was far behind got the
  session's whole event log loaded into memory at once. It now arrives in
  pages.
- `GET /session/{id}/message` without `limit` built the whole history in
  memory before answering. A history of 100 messages or more is now written
  out a page at a time. Clients that can should still pass `limit` and page
  with `before`.
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

- Databases written by earlier builds of this branch are repaired on first
  open (migration `0008_session_mark_backfill`). `memory_0002_provenance`
  had been edited in place under the same id, and builds from before the
  session marks tagged a trigger's session with `metadata.triggerID` only: a
  database that ran one of them kept webhook and watch sessions, or their
  subagents, unmarked, and what they had saved to memory verified. Now a
  session a trigger started carries both marks and the read-only ceiling
  unless its trigger is still stored and is a `cron`, `interval` or `at`
  trigger (a deleted trigger, one only the config declares, or a spec that
  does not parse proves nothing); descendants of a marked session are
  marked; and a memory record whose writing session is marked or gone is
  unverified. A scheduled trigger declared in the config whose `reuse`
  session is caught by this starts a fresh one once that session is
  archived. Migration bodies are now pinned by hash in the tests, so an
  edit to a shipped migration fails CI.

### Changed

- A `lock/` directory appears in the data directory, holding empty lock
  files (`scheduler-<hash>.lock` per project directory, `maintenance.lock`),
  and a restart marker that is being dealt with is briefly renamed to
  `running/<session>.….claim`. A process alone on its data directory always
  holds the locks and wins every claim, and behaves as before. With several
  processes on one data directory (a TUI beside `alforria serve`), one of
  them now schedules each directory's triggers and maintains the database
  and the others are followers, where each used to do both on its own; a
  follower takes over within seconds of the leader's exit. Migration
  `0010_trigger_run_owner` adds two columns and an index to `trigger_run`.
  When a directory's instance is booted again inside one server (after
  `/instance/dispose` or a config change), the new instance's scheduler takes the
  place of the old instance's, which stops firing.
- On macOS and other non-Linux Unix systems the owner of a restart marker is
  now checked with a signal-0 probe, so a turn a killed server left mid-way
  is repaired at the next start there too (it was only on Linux).

- A `hook` bearer credential must be bound to one trigger (`trg` claim, or
  `trigger` on a static token) and delivers only to it; an unbound `hook`
  token is refused. A webhook trigger accepts one delivery per
  `triggers.webhook_min_interval` (default `"1s"`).

- `alforria serve` / `web` refuse to listen on a host config that does not
  parse **or cannot be read** (an unreadable file in the global config
  directory, `~/.opencode` or the managed directory used to count as empty,
  silently dropping the policy in it), and — with bearer auth configured —
  when a directory tokens may open contains the data directory or the host
  config.

- A host or managed `permission` rule is a ceiling: a project's
  `permission` or `agent.<name>.permission` can narrow it and no longer
  widens it (upstream's last-match-wins let a project file overrule a host
  `deny`). Installs whose own config sets no `ask`/`deny` see no difference.
- On a server with `server.auth.bearer` configured, a project's or a remote
  config keeps only keys that cannot carry a capability; `permission`, `mcp`,
  `plugin`, `lsp`, `formatter`, `instructions`, provider options and the like
  are dropped with a warning unless the host lists them in
  `server.trust.project_keys`. Standalone installs are unchanged. See
  `docs/unattended-server.md`.
- A restricted session, or one that outside input started (a webhook or
  watch run), cannot write `opencode.json`, `.opencode/`, `AGENTS.md` and
  the other loader files with
  its edit tools (`permission.edit_loader: "allow"` in the host config hands
  it back). Such a write, and an upload, is made on the resolved path that
  was judged, through its directory's handle with no link followed, so a
  rename or link swap between the check and the write cannot redirect it
  (Unix; what the guard does not cover is listed in
  `docs/unattended-server.md`). The guard is the same wherever such a
  session runs: `alforria run --session` on one protects the same resolved
  locations as the server. Reverting or unreverting such a session is
  refused (`403`, nothing restored) when it would rewrite or delete one of
  those files, and otherwise restores exactly the files that were checked
  (git-ignored files and symlinks are left as they are); other sessions
  revert as before.
- When the global config does not load, a project's capability-bearing keys
  are not applied until it does (the load itself still degrades to defaults,
  as before).

- On a server where project config is restricted (`server.auth.bearer`
  configured, or `server.trust.project: "restricted"`), `{env:NAME}` and
  `{file:path}` in a **project's** `opencode.json` / `.opencode/` and in a
  remote config are confined: `{env:}` reads only the names in
  `server.trust.project_env` (anything else is the empty string), and
  `{file:}` only a relative path that stays inside the project (anything
  else is a config error; a remote config reads no file). `${VAR}` in a
  provider base URL follows the `{env:}` rule, and a refused name there is a
  config error. **A standalone install is unchanged**: a project config's
  `{env:OPENAI_API_KEY}` or `{file:~/.secrets/key}` works as upstream, and a
  warning is logged once per file and reference naming what a restricted
  server would refuse. Your own config is never confined.

- `server.resume` and `storage` (`retention`, `synchronous`) are read from
  the host config only (the global config directory, `~/.opencode`, the
  managed directory), like `server.auth` and `shell_env`. A host key written
  in a project file, a remote config or an `OPENCODE_CONFIG*` source is
  dropped with a warning naming the file.
- A session that carries outside text (a webhook or watch run, its children
  and forks) is held to its own rules: the project's standing `always`
  approvals do not apply to it, an `ask` there is a deny at once, and
  `permission.unattended` never answers for it. A fork of such a session
  keeps its rules.
- `@file` and `@agent` mentions in a trigger's prompt are resolved only when
  the operator wrote the trigger (the config or an `admin` request). In a
  trigger a model scheduled they are text.
- A session a trigger names must belong to the trigger's own directory.
- A trigger run whose turn is stopped is recorded as failed instead of
  completed with a partial answer; a run the server died in is closed with
  an `interrupted` error.
- A memory record written by a `chat` or `connector` token, or by a session
  one of them created, is injected into restricted sessions only.
- Memory in a directory that is not a git repository is kept per directory.
  All such directories are one project (`global`), and they used to share
  one set of records and one transcript search — which also let a token
  confined to one directory read, change and delete another's. Now each has
  its own records and searches its own sessions; a repository's memory is
  still its project's, shared by its worktrees and subdirectories. **If you
  had turned memory on and used it outside a repository**, you can notice
  this: after the upgrade a directory shows the records that were saved *in
  it* (the `0009_memory_directory` migration attributes each existing record
  through the session that wrote it) and no longer those saved in another
  non-repository directory. Records with no session to go by — saved
  through `POST /memory`, or by a session deleted since — are shown in no
  directory until you file them: `GET /memory/unattributed`, then
  `PATCH /memory/unattributed/{id}?directory=…` or `DELETE` (Basic auth, an
  `admin` token, or no authentication). `memory.scope: "global"` still
  shares one memory across everything. Memory is off by default; an install
  that never enabled it sees no change. See `docs/assistant.md`.
- `git` run by the server (status, diff, snapshots) no longer inherits the
  server's secrets: a repository's own filters and hooks see the scrubbed
  environment.
- On SIGTERM the trigger schedulers stop firing and the transcript indexers
  end before the running turns are settled, and a turn that ended inside the
  process leaves no restart marker.
- The agent's processes no longer inherit alforria's own secrets and provider
  API keys; everything else in the environment is passed as before on a
  standalone install (`GITHUB_TOKEN`, `AWS_*`, `NPM_TOKEN`, `PGPASSWORD` and
  the like still reach `gh`, `aws`, `npm` and `psql`). The `bash` tool, the
  session `shell` command, command-template `` !`cmd` `` substitutions,
  terminals, worktree start scripts, MCP stdio servers, language servers and
  the server's `git` calls start without: the server password and auth
  variables (`OPENCODE_SERVER_PASSWORD`, `OPENCODE_AUTH_CONTENT`,
  `OPENCODE_CONFIG_CONTENT`, `ALFORRIA_*_SECRET` / `_TOKEN` / `_KEY` /
  `_PASSWORD`); the built-in provider key variables (`ANTHROPIC_API_KEY`,
  `OPENAI_API_KEY`, `LIBERTAI_API_KEY`, …); any variable named `*_API_KEY`
  or `*_API_TOKEN`; every `provider.*.env` name of the model catalog and of
  a loaded config; and every variable a config file reads through
  `{env:NAME}` — in any letter case. A name learned from a config or the
  catalog stays withheld until the server restarts. To pass one through,
  name it in `shell_env.allow`, or in an MCP server's own `environment`.
- A standalone user who uses one of these gets the restricted shell
  environment (an allowlist; see `shell_env` under Added) and will find
  `GITHUB_TOKEN`, `AWS_*` and similar missing from the agent's shell until
  `shell_env.allow` names them: `alforria serve --hostname` with an address
  other than loopback (`0.0.0.0`, a LAN address), or a `webhook` / `watch`
  trigger (in the config, or created by the `schedule` tool — from the
  moment it is created, until the server restarts without it).
- A global config file that does not parse now stops `alforria serve` and
  `alforria web` from listening, instead of serving with the defaults: the
  host config carries the server's access policy.
- The default system prompts call the agent `alforria` instead of `opencode`
  and point at alforria's repository and docs. A model told it was opencode
  will now say it is alforria.
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

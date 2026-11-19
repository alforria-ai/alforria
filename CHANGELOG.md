# Changelog

All notable changes to alforria are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). alforria is
pre-1.0, so minor releases may include breaking changes.

## [Unreleased]

### Added

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
  project's, and a webhook run cannot act unless its trigger allows it.
  Triggers are off until there is one: with `triggers.enabled` unset, a
  directory with no trigger (in the config or stored) runs no scheduler,
  and the first `POST /trigger` starts it; `triggers.enabled: true` starts
  it with nothing scheduled, and `false` turns the feature off. At startup
  `alforria serve` boots the directories that hold a stored trigger (and,
  when the config asks for triggers, its own directory) so they fire
  without a client; with no trigger it boots none, as before. See
  `docs/assistant.md`.
- A `schedule` tool, so an agent can create, list, change, pause and delete
  its own triggers and one-shot reminders, gated by a `schedule` permission.
  It only changes triggers its own conversation scheduled, and what it
  schedules runs under the rules its creator had. It is offered once the
  directory has a trigger, or with `triggers.enabled: true`: a request from
  someone who uses no trigger carries the same tools as before.
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
  history behind a compaction boundary carrying the transcript's `summary`
  (fenced as text from outside), so the next prompt sends only the summary
  and the live messages. `--dry-run` reports what would be imported, an
  `import_id` makes the import idempotent, and `--untrusted` marks the
  session as started by outside input. The file's `metadata` is kept except
  for the keys the server reserves (`triggerID`, `untrustedInput`).
- Persona controls for products that embed alforria: `prompt.environment`
  (`true`, `false` or a template) for the `<env>` block,
  `prompt.model_identity` and `prompt.empty_skills` for the other fixed
  additions, and `tool.<name>.description` / `description_append` to reword
  a built-in tool.
- `provider.<id>.models.<name>.family` picks the default system prompt for a
  model whose id does not say what it is (a family name or the underlying
  model id).
- The `websearch` tool can use LibertAI search
  (`websearch: { provider: "libertai" }`, with the `libertai` provider's
  key) or any endpoint with the same contract
  (`websearch: { provider: "http", url, headers }`), and is then offered to
  every model. Without `websearch.provider` nothing changes: the tool is
  offered to the same models and answered by the same providers as before,
  whether or not a LibertAI key is present.
- The built-in `libertai` provider accepts its catalog's aliases
  (`claw-flash`, `claw-core`, `deep-claw`, …) as model ids, with the target
  model's limits, cost, capabilities and prompt, and lists
  `deepseek-v4.1-flash-1017` and its `-thinking` variant.
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
  anything is written, from a running count per directory that is re-read
  at most every 5 minutes and after a restart (what an upload replaces is
  read as it is renamed into place, so concurrent uploads to one file
  cannot lower the count below what is there). The write guard also protects the files a `formatter` or
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
  server runs, and is shown by `GET /server/posture` (admin). The shells of
  a run that a webhook or watch trigger started, and of a restricted
  session — its `bash` tool, the `shell` command and inline template
  shell — always get the restricted environment. In either posture the
  server's own secrets, provider key variables, `*_API_KEY` /
  `*_API_TOKEN` names, the names learned from configs and the key
  variables of the catalog providers in use are withheld (see Changed).
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
- Scoped tokens reach the assistant features. A `chat` or `connector` token
  manages its own schedules (`/trigger*`) under the `schedule` tool's rules —
  it never sets a trigger's `permission`, `agent` or `model`, names only
  sessions it may act in, and changes or fires only triggers a restricted
  caller created — reads and writes memory (`/memory*`) under the `memory`
  permission, and may replay a session's durable events
  (`GET /api/session/{id}/event`). A new `hook` scope opens
  `POST /trigger/{id}/hook` and nothing else, for whatever calls a webhook
  from outside: such a credential is bound to one trigger (`trg` claim, or
  `trigger` on a static token). `connector` and `admin` may call any
  webhook trigger, `chat` none.
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

- An MCP OAuth sign-in started the desktop's URL handler (`xdg-open`,
  `open`, `cmd /c start`) on whatever authorization URL the MCP server's
  metadata named, with the server's whole environment, on any server. It
  now goes through the same check as every other sign-in — a plain
  `http(s)` URL or nothing — and a server in the restricted shell posture
  starts no opener at all: the flow reports `mcp.browser.open.failed` with
  the URL, as it does when a browser cannot be opened.
- File tickets shared one cache of 10 000 with oldest-first eviction, so
  one token minting tickets in a loop invalidated everybody else's. A
  credential now holds at most 1 000 live tickets and evicts its own
  oldest beyond that.
- `GET /file/content` checked that the path existed and then read it by
  name on an async worker, into memory, whatever it was: a FIFO in the
  project (the agent's shell can make one) parked a server thread for
  ever, and a sparse 50 GiB file was read whole. The file routes and the
  inlined file part of a prompt now open the file once without waiting,
  serve only a regular file (`400` otherwise), read off the async workers,
  and cap what is returned in one piece — `server.upload.max_bytes` for
  `GET /file/content` (`413`), 32 MiB for a prompt's file part. For a
  credential below `admin` or a ticket the file is opened beneath the
  project root it was judged inside instead of by name again
  (`/file/raw`, `/file/content`, `/file` list, prompt file parts). Also:
  the frozen `/file` list confines a request with no principal instead of
  trusting it; an upload onto a dangling symlink named like a loader file
  (`uploads/AGENTS.md -> /nonexistent`) is refused instead of creating a
  real `AGENTS.md`; and `overwrite=false` is decided by a no-replace
  rename, so concurrent uploads of one new file create it once (`409` for
  the rest).
- Uploads still being received were outside the upload quota: the body
  was written to its temporary file before anything was counted, so any
  number of slow uploads could each be admitted into the same free space,
  an upload whose client went away left its temporary file behind, and one
  that stopped sending held its connection for ever. An upload below
  `admin` now holds the bytes it declares (or has sent) against the quota
  until it is committed or ends; an upload directory receives at most 8 at
  a time, 4 of one credential (`429 TooManyUploadsError` for one more); a
  body that sends nothing for 30 seconds, or is slower overall than
  64 KiB/s over `max_bytes` plus a minute, is ended
  (`408 UploadTimeoutError`, for every caller); the temporary file is unlinked however the upload ends; and
  temporary files a killed server left are removed at the next count.
- Upload quota: replacing a file took the file's size on disk off the
  running count, whatever had been counted for it. In a full upload
  directory the agent's shell could make a file sparse and huge
  (`truncate -s 100G uploads/pad`), a one-byte upload over it then
  credited the whole 100G, and the directory was "empty" again. A replace
  now takes off what the count holds for that file and no more, and a
  difference has the directory counted again before the next upload. An
  operator's upload into an upload directory is counted before the next
  upload below `admin` instead of up to five minutes later, and a
  directory in the area that cannot be listed makes the area full instead
  of counting as empty.
- A restricted session's `read`, `grep` and `glob` could open the host's
  own files — `auth.json` and the rest of the data directory, the global
  config with its webhook secrets and MCP headers: the only thing in the
  way was the `external_directory` ask, which the `chat` token that owns
  the session answers itself with `once` (or
  `permission.unattended.on_timeout: "allow"` answers for it). A session
  that is restricted or was started by outside input is now refused those
  locations by the tools, with no ask, and is attached no file from them;
  a search that starts above one returns nothing from inside it. The saved
  tool outputs, plans, skill and reference directories stay readable. The
  operator's own sessions are unchanged.
- A running server applied its host config as it was on disk each time a
  directory was first opened, while the access policy was read once at
  startup. A global config that stopped parsing (or lost a `{file:}` it
  refers to) counted as empty for the next directory — the host's
  `permission` ceiling gone, the built-in `*: allow` in its place — and an
  agent with a shell could rewrite the host files to drop bearer auth or
  set `server.trust.project: "full"` and have the next directory opened
  believe it. A server that is unattended, reachable or project-restricted
  now keeps what the host sources loaded as at startup, and how far project
  config is believed, for every directory it opens; editing a host file on
  such a server takes a restart (`PATCH /global/config` still applies:
  what the request sends, and nothing read back from disk). A
  developer's own server reads its host files at each open as before, and
  keeps the last reading that loaded when one stops parsing instead of
  running with none. In a server process (the TUI's own included) a global
  config that has never loaded is now an error when a directory is opened,
  where it used to be an empty config with a warning; `alforria run` and
  the other one-shot commands still degrade it as upstream does.
- On a server where project config is restricted (bearer auth, or
  `server.trust.project: "restricted"`), a project file could still step
  around the rules the host wrote per agent: define an agent of its own
  (built from the built-in defaults, `*: allow`, with none of
  `agent.build.permission`), name it `default_agent`, or flip a host
  agent's `mode` / `hidden`. A restricted project config can no longer set
  `default_agent`, `agent.*.mode`, `agent.*.hidden` or `agent.*.disable`,
  a `mode.<name>` entry is read as `agent.<name>` without the `primary` it
  implies, and an agent only a project or remote config defines — JSON or
  markdown under `.opencode/` — is not loaded unless the host lists
  `agent.<name>` in `server.trust.project_keys`. Standalone installs are
  unchanged.
- A process whose shell posture was restricted from startup (bearer auth,
  `permission.unattended`, a webhook) could be put back in the standalone
  posture by a later policy install in the same process: only a posture
  tightened after startup was kept. Installing a policy now never relaxes
  the posture, however it became restricted.
- An MCP tool whose key spelled a built-in tool's name (a server `apply`
  with a tool `patch` is `apply_patch`) was classified as that built-in
  when deciding whether its answer is outside content, and could leave the
  session unmarked. MCP and plugin tools are now judged by where they come
  from: whatever they answer marks the session, under any name.
- `POST /session` accepted any `parentID` from a token below `admin`, so
  a `chat` client could create a session as the child of an operator's
  session. It is refused (`403`) unless the parent is a session the token
  may act on, as on every route that names a session in its path.
- Forking a session while one of its turns was running could produce a
  fork that held content without what the content had brought: the fork
  took its source's marks, ceilings and restriction first and copied the
  messages afterwards, so a tool result that marked the source in between
  was copied into an unmarked fork. The source is now read again after the
  copy and the fork brought up to it; a fork that cannot be is removed
  and the request fails.
- Answering one permission ask with `always` also answered the session's
  other pending asks that the new approval covered, judged by the
  approvals alone. An ask that was pending because a sealed ceiling (a
  trigger creator's bounds) still asked about it was released by it,
  unanswered. Each pending ask is now judged again under the policy it was
  asked under, as a new ask of the same thing would be. Sessions without
  ceilings behave as before.
- A trigger scheduled by a run that outside input had started (a webhook
  or watch trigger whose `permission` block grants `schedule`) recorded
  its creator's restriction and outside-content provenance, but not that
  outside input had started it. Its own runs were ordinary ones: their
  sessions had no outside-start mark, so no write guard, no restricted
  shell environment and no add-only memory. The trigger now records it
  (`outsideStart`), and its runs in a session of their own are started by
  outside input: marked, and read-only. A `current` reminder still runs in
  the session that scheduled it, under that session's own ceiling.
- A subagent resumed with `task_id` went on under the ceilings its session
  was created with, even when its parent's had been narrowed since (a
  trigger's bounds are replaced at each run): the resume compared the two
  sessions' deny rules only. A resumed subagent is now given its parent's
  current ceilings before it is prompted.
- Three tools handed an operator's session what a client below `admin`
  had written without marking the session as having seen outside content:
  `memory` (`search`, `list`) returning a record a restricted writer
  saved, `session_search` returning an excerpt of a restricted session,
  and `schedule` (`list`, and the other actions' answers) printing the
  prompt of a trigger a restricted caller made or one written after
  outside content was seen. Each now flags such a result, and the session
  is marked: what it then saves to memory is unverified, and a trigger it
  schedules carries the mark. A restricted session reading its own kind's
  records, excerpts and triggers is not marked by them.
- A command configured to run as a subagent (`subtask: true`) stored the
  subagent's answer in the session without the outside-content check every
  other tool result gets: a subagent that had fetched a page or read an
  upload left the session that received its answer unmarked. It is marked
  now, as by a `task` call the model makes.
- An `@file` mention in a prompt the model wrote for a subagent or a
  workflow agent attached any file in the project with no permission
  asked: `Summarise @.env` handed the subagent a file its `read` rule asks
  about or denies. A mentioned file is now attached only when the child
  session's policy allows that agent to `read` it; otherwise the mention
  stays text and the agent's own `read` tool asks as usual. A file from
  the upload area attached this way marks the child session as having seen
  outside content, as reading it would (and through it the parent, when
  the result comes back). An `@agent` mention in such a prompt stays text:
  it used to become an agent part, which let the subagent start that agent
  without the `task` ask. Prompts you type resolve as before.
- Reverting or unreverting a guarded session (restricted, or started by
  outside input) could write or delete a file outside the project: the
  guard judged each file against the protected locations only, so a file
  under a directory the agent had since replaced with a link out of the
  project was restored or removed where the link led. Such a revert is now
  refused (`403`, nothing restored), naming the files. Other sessions'
  reverts already stayed inside the worktree.
- Two alforria processes starting at the same moment on a fresh install could
  both try to create the database, and the second failed with `table
  workspace already exists`. The second now waits and uses the database the
  first one created.
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
- A trigger run whose turn panicked stayed `running` for as long as the
  server lived, and every later fire of the trigger was skipped, in every
  server sharing the data directory. It is now a failed run (`the run
  panicked: …`) and the trigger is free again.
- A trigger that could never run sensibly is refused when it is created,
  changed or read from the config (where it is listed with the reason): a
  `cron` expression no date matches (`0 0 30 2 *`), a `watch` `debounce`
  under 1 second (`0ms` fired a run per file event), and an `interval` or
  `debounce` over 366 days.
- A stored trigger whose record does not say whether a restricted caller
  created it is read as restricted, not as the operator's. `restricted` is
  now always written, `false` included. (Existing stored triggers are
  affected: see Changed (b).)
- Starting an instance read and parsed every trigger run its directory had
  kept (up to 200 a trigger) to find the unfinished ones. It now reads the
  unsettled runs only, through the index that already marks them.
- `provider.<id>.models.<name>.family` in one project's config decided the
  system prompt of a model of the same name in every other project the
  server had open (it was kept in a process-wide table). It now applies to
  the instance whose config declares it.

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
  as text-only.
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
  unverified. What this over-marks is under Changed (b). Migration bodies
  are now pinned by hash in the tests, so an edit to a shipped migration
  fails CI.
- Smaller findings of the security review:
  - The Basic username and password are compared in constant time.
  - `GET /memory/search` and `GET /memory/session` answer `400` to a query
    the index cannot read (a NUL character, a full-text syntax error)
    instead of `500`.
  - A memory update that changes only `kind` no longer makes the record its
    changer's: a note from a token below `admin` that a session's `memory`
    tool re-kinded became a verified tool record, injected into the
    operator's sessions. The origin now stands (and a model's rewrite of a
    restricted writer's record stays restricted); the operator's own change
    through `/memory` still vouches.
  - `{:?}` of a webhook (and so of the host config and the server policy) no
    longer prints its `secret`, its `signing_key`, or more of its URL than
    the host.
  - A request that reaches a handler without a principal is no longer taken
    for the operator: the auth layer now names the caller on every request
    it lets through (the operator, on a server with no password and no
    tokens), and a request without one is held to what a token below `admin`
    may do, and logged.
  - The `/memory` and `/trigger` routes do their storage work on blocking
    threads instead of the runtime's workers, so a slow write or a busy
    trigger service no longer stalls unrelated requests.

### Changed

Everything an existing install can notice, in three groups: (a) what
changes for someone who configures none of the new features; (b) what
changes for someone who ran an earlier build of this branch with triggers,
memory, tokens or hooks; (c) what applies once bearer auth, a trigger or
memory is configured.

#### (a) For everyone, with none of the new features configured

- **The agent's processes no longer see provider keys.** The `bash` tool,
  the session `shell` command, command-template `` !`cmd` `` substitutions,
  terminals, worktree start scripts, MCP stdio servers, language servers
  and the server's own `git` calls (status, diff, snapshots — so a
  repository's filters and hooks too) start without: the server password
  and auth variables (`OPENCODE_SERVER_PASSWORD`, `OPENCODE_AUTH_CONTENT`,
  `OPENCODE_CONFIG_CONTENT`, `ALFORRIA_*_SECRET` / `_TOKEN` / `_KEY` /
  `_PASSWORD`); the built-in provider key variables (`ANTHROPIC_API_KEY`,
  `OPENAI_API_KEY`, `LIBERTAI_API_KEY`, …); **any variable named
  `*_API_KEY` or `*_API_TOKEN`**, a provider's or not (`STRIPE_API_KEY`
  goes too); every `provider.*.env` name of a loaded config; every
  variable a config file reads through `{env:NAME}` — in any letter case;
  and, of the names the model catalog lists, the credential-shaped ones
  (`*_TOKEN`, `*_KEY`, `*_SECRET`, …) of the providers in use (declared
  or selected in a config, signed in to, keyed from a variable set in the
  server's environment, or selected by a session — `HF_TOKEN` goes when
  it is set). Everything else in
  the environment is passed as before: `GITHUB_TOKEN`, `GH_TOKEN`,
  `GITLAB_TOKEN`, `AWS_*`, `AZURE_*`, `GOOGLE_APPLICATION_CREDENTIALS`,
  `GOOGLE_CLOUD_*` and `GCLOUD_*` always — even when GitHub Copilot or
  Amazon Bedrock, which the catalog lists them for, is the provider in
  use, so `gh` and `aws` keep their credentials in the agent's shell —
  and `NPM_TOKEN`, `PGPASSWORD`, `AWS_PROFILE` and the like; so are the
  regions, projects and endpoints the catalog lists. A name learned from a config or the catalog
  stays withheld until the server restarts. To pass a withheld one
  through, name it in `shell_env.allow` in the global config
  (`"shell_env": { "allow": ["HF_TOKEN"] }`), or in an MCP
  server's own `environment`; to hide a passed one (a Copilot or Bedrock
  user's `GITHUB_TOKEN` / `AWS_*`), name it in `shell_env.deny`.
- **Binding beyond loopback restricts the shell environment further.**
  `alforria serve --hostname` with anything other than `localhost` or a
  loopback address (`0.0.0.0`, a LAN address) starts the agent's processes
  from an allowlist (`PATH`, `HOME`, `LANG`, `LC_*`, `TERM`, `TZ`, `USER`,
  `LOGNAME`, `SHELL`, `TMPDIR`): `GH_TOKEN`, `AWS_*`, `SSH_AUTH_SOCK`,
  proxy variables and the rest are missing until `shell_env.allow` names
  them. See `shell_env` under Added for the other things that select this
  posture.
- **A host or managed `permission` rule is a ceiling.** A project's
  `permission` or `agent.<name>.permission` can narrow it and no longer
  widens it (upstream's last-match-wins let a project file overrule a host
  `deny`). Installs whose own config sets no `ask`/`deny` see no
  difference.
- **A host config that does not load is no longer treated as empty.**
  `alforria serve` and `alforria web` refuse to listen when a file in the
  global config directory, `~/.opencode` or the managed directory does not
  parse or cannot be read (they used to serve with the defaults, silently
  dropping the policy in it). An unreadable file in the managed directory
  fails every config load, the TUI's and `alforria run`'s included.
  Elsewhere the load still degrades to defaults as before, but a project's
  capability-bearing keys (`permission`, `mcp`, `plugin`, …) are not
  applied until the global config loads.
- **A custom command's arguments are never shell source** (see Fixed).
  Templates that relied on an argument being pasted into a `` !`…` `` as
  shell source, rather than as a value, need rewriting with `$1` /
  `$ARGUMENTS`.
- **A custom model that declares no image input is text-only** (see
  Fixed): a vision model you added to the config must list the `image`
  input modality or set `attachment: true`, or its images are replaced by
  a placeholder.
- **A model that declares no context window is sized against 128,000
  tokens** (`limit.context` of 0 — a model you added to the config without
  one): what is sent to it is bounded to that, where the whole
  conversation used to go out whatever its length. Such a model is still
  not treated as overflowing for automatic compaction.
  `compaction.default_context` sets another size; models that declare a
  window are unaffected.
- **`serve` and `web` handle SIGTERM and SIGINT, and repair after a
  kill** (both under Added): a stop is now a clean shutdown — running
  turns aborted and recorded, tool processes killed, exit 0 — that can take
  up to `server.shutdown.timeout` (10 s by default), and a session left
  mid-turn by a kill is idle with its tool calls closed as errors at the
  next start, instead of looking busy.
- **The write-ahead log is checkpointed every five minutes** (a truncating
  checkpoint; `serve` and `web`, and once at startup for the TUI's built-in
  server), so the `-wal` file no longer sits at its high-water mark. It
  used to be folded back only as SQLite saw fit and when the database was
  closed.
- **New files in the data directory.** `lock/maintenance.lock` (an empty
  lock file; also `lock/scheduler-<hash>.lock` per project directory once
  it has a trigger and `lock/outbox.lock` with webhooks), and `running/`,
  holding one small marker per running turn; a marker being dealt with
  after a kill is briefly renamed `running/<session>.….claim`. A process
  alone on its data directory always holds the locks and wins every claim.
- **New tables in the database.** alforria's own migrations
  (`0001`–`0011`, journal table `alforria_migration`) run on first open;
  the upstream schema is untouched. On a database that used none of the
  features they create empty tables. A session gets one row in
  `session_mark` the first time a web, MCP or plugin tool answers in it;
  with memory and triggers off nothing reads it.
- **`triggerID` and `untrustedInput` are reserved session metadata keys**:
  a client that sends them on session create or update has them dropped.
- **A route naming a session and a message, part or permission request
  answers 404** when the inner id belongs to another session (see Fixed).
- The default system prompts call the agent `alforria` instead of `opencode`
  and point at alforria's repository and docs. A model told it was opencode
  will now say it is alforria.

#### (b) If you ran an earlier build of this branch with memory, triggers, tokens or hooks

- **Memory outside a repository is kept per directory.** All directories
  that are not a git repository are one project (`global`), and they used
  to share one set of records and one transcript search — which also let a
  token confined to one directory read, change and delete another's. After
  the upgrade a directory shows the records that were saved *in it* (the
  `0009_memory_directory` migration attributes each existing record
  through the session that wrote it) and no longer those saved in another
  non-repository directory. Records with no session to go by — saved
  through `POST /memory`, or by a session deleted since — are shown in no
  directory until you file them: `GET /memory/unattributed`, then
  `PATCH /memory/unattributed/{id}?directory=…` or `DELETE` (Basic auth, an
  `admin` token, or no authentication). A repository's memory is still its
  project's, and `memory.scope: "global"` still shares one memory across
  everything.
- **Past trigger sessions and their memory are marked on upgrade**
  (migration `0008_session_mark_backfill`, failing closed). A session a
  trigger started carries both marks and the read-only ceiling unless its
  trigger is still stored and is a `cron`, `interval` or `at` trigger — a
  deleted trigger, one only the config declares, or a spec that does not
  parse proves nothing. So the past sessions of a *scheduled* trigger
  declared in the config are over-marked: they are read-only, and such a
  trigger's `reuse` session stays so until you archive it, after which the
  trigger starts a fresh one. A memory record whose writing session is
  marked or gone becomes unverified and is no longer injected; rewrite it
  through `/memory` to vouch for it again.
- **A stored trigger whose record does not say `restricted` is read as
  restricted.** Earlier builds left the field out when it was false, so
  the triggers you created through `POST /trigger` or the `schedule` tool
  now create restricted sessions, resolve no `@file` / `@agent` mention,
  can be changed, fired and deleted by a `chat` or `connector` token, and
  fail their runs if they name a session of yours. `restricted` is now
  always written and never changed: delete such a trigger and create it
  again as the operator.
- **Which session a trigger may name is restricted** (see Fixed). A
  `webhook` or `watch` trigger, or a run with a payload, names none; a
  trigger the `schedule` tool made names only the conversation that
  scheduled it (`current`); a session a trigger names must belong to the
  trigger's own directory. A stored trigger that no longer fits is not
  rewritten: its runs are recorded as `failed` with the reason until you
  change it.
- **Triggers and the `schedule` tool are off until a directory has a
  trigger.** With `triggers.enabled` unset, earlier builds ran a scheduler
  and offered the `schedule` tool everywhere. An agent that was expected to
  schedule its *first* trigger itself now needs `triggers.enabled: true`.
  `serve` no longer boots its own directory at startup unless the config
  asks for triggers.
- **`websearch` uses LibertAI search only when the config says so**
  (`websearch: { provider: "libertai" }`). Earlier builds switched to it,
  and offered the tool to every model, as soon as a LibertAI key was
  present; unset is upstream's behaviour again.
- **A `hook` bearer credential must be bound to one trigger** (`trg`
  claim, or `trigger` on a static token) and delivers only to it; an
  unbound `hook` token is refused. A webhook trigger accepts one delivery
  per `triggers.webhook_min_interval` (default `"1s"`).
- **`GET /attention`, `GET /permission` and `GET /question` show a token
  below `admin` only the asks it may answer** — those of restricted
  sessions in the directories it may open — and the `outbox` counters on
  `GET /attention` are the operator's only.
- **Webhook deliveries go out per hook, in order.** A hook's later
  deliveries wait behind one that is being retried; a 4xx other than 408,
  425 and 429 is given up at once instead of retried until `max_age`; of
  several servers on one data directory, one delivers.
- **Several processes on one data directory** (a TUI beside
  `alforria serve`) no longer each schedule and maintain: one schedules
  each directory's triggers and one maintains the database, the others are
  followers and take over within seconds of the leader's exit; every fire
  is claimed once. Migration `0010_trigger_run_owner` adds two columns and
  an index to `trigger_run`, and `0011_trigger_run_settle_unowned` closes
  as failed the runs an earlier build left `queued` or `running`, which
  the startup pass no longer reads. When a directory's instance is booted again
  inside one server (after `/instance/dispose` or a config change), the new
  instance's scheduler takes the place of the old one's, which stops
  firing. On macOS and other non-Linux Unix systems a restart marker's
  owner is now checked with a signal-0 probe, so the repair after a kill
  works there too (it was Linux only).
- **A trigger run that does not finish is `failed`**, not completed with a
  partial answer: one whose turn is stopped, one whose turn panics, and one
  the server died in (closed with an `interrupted` error). On SIGTERM the
  schedulers stop firing before the running turns are settled.
- **`@file` and `@agent` mentions in a trigger's prompt** are resolved only
  when the operator wrote the trigger (the config or an `admin` request).
  In a trigger a model scheduled or a restricted caller created they are
  text.
- **A memory record written by a `chat` or `connector` token**, or by a
  session one of them created, is injected into restricted sessions only;
  a change of `kind` alone no longer makes a record its changer's.
- **`server.resume` and `storage`** (`retention`, `synchronous`) are read
  from the host config only (the global config directory, `~/.opencode`,
  the managed directory), like `server.auth` and `shell_env`. A host key
  written in a project file, a remote config or an `OPENCODE_CONFIG*`
  source is dropped with a warning naming the file.
- **A `webhook` or `watch` trigger restricts the shell environment** of the
  whole server from the moment it exists — in the config, or created over
  HTTP or by the `schedule` tool — until the server restarts without it:
  `GH_TOKEN`, `AWS_*` and similar are missing from the agent's shell until
  `shell_env.allow` names them.

#### (c) Once bearer auth, a trigger or memory is configured

- On a server with `server.auth.bearer` configured, a project's or a remote
  config keeps only keys that cannot carry a capability; `permission`, `mcp`,
  `plugin`, `lsp`, `formatter`, `instructions`, provider options and the like
  are dropped with a warning unless the host lists them in
  `server.trust.project_keys`. `{env:NAME}` and `{file:path}` in such a
  config are confined too: `{env:}` reads only the names in
  `server.trust.project_env` (anything else is the empty string), and
  `{file:}` only a relative path that stays inside the project (anything
  else is a config error; a remote config reads no file). `${VAR}` in a
  provider base URL follows the `{env:}` rule, and a refused name there is
  a config error. `server.trust.project: "restricted"` selects the same
  without bearer auth. **A standalone install is unchanged**: a project
  config's `{env:OPENAI_API_KEY}` or `{file:~/.secrets/key}` works as
  upstream, and a warning is logged once per file and reference naming
  what a restricted server would refuse. Your own config is never
  confined. See `docs/unattended-server.md`.
- With bearer auth configured, `serve` and `web` refuse to listen when a
  directory tokens may open contains, is, or lies inside the data
  directory or the host config.
- A restricted session, or one that outside input started (a webhook or
  watch run), cannot write `opencode.json`, `.opencode/`, `AGENTS.md` and
  the other loader files with its edit tools
  (`permission.edit_loader: "allow"` in the host config hands it back).
  Such a write, and an upload, is made on the resolved path that was
  judged, through its directory's handle with no link followed, so a
  rename or link swap between the check and the write cannot redirect it
  (Unix). The guard is the same wherever such a session runs:
  `alforria run --session` on one protects the same resolved locations as
  the server. Reverting or unreverting such a session is refused (`403`,
  nothing restored) when it would rewrite or delete one of those files,
  and otherwise restores exactly the files that were checked (git-ignored
  files and symlinks are left as they are); other sessions revert as
  before. What the guard does not cover — `bash` and every other process
  first of all — is listed under "The write guard" in
  `docs/unattended-server.md`.
- A session that carries outside text (a webhook or watch run, its children
  and forks) is held to its own rules: the project's standing `always`
  approvals do not apply to it, an `ask` there is a deny at once, and
  `permission.unattended` never answers for it. A fork of such a session
  keeps its rules. Its `bash` tool gets the restricted shell environment
  whatever the server's posture, as a restricted session's does.

## [0.4.4] - 2026-11-13

### Fixed

- A turn in a project with many ignored or changed files could hang for good
  before reaching the model, and every later turn in that project with it:
  the snapshot fed git its file list before reading git's answer, and both
  sides blocked once the answer filled the pipe.

## [0.4.3] - 2026-11-12

0.4.2 was tagged but shipped no binaries: its macOS verification caught the
first item below. Everything listed under 0.4.2 is in this release.

### Fixed

- A project whose path goes through a symlink (on macOS, anything under
  `/home` or `/tmp`) had its own files judged outside it.
- `bash` rules also see a command glued to a wrapper's flag (`env -S"rm …"`,
  `env -iSrm …`), and a wrapper command too long to check is refused.

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

[Unreleased]: https://github.com/alforria-ai/alforria/compare/v0.4.4...HEAD
[0.4.4]: https://github.com/alforria-ai/alforria/compare/v0.4.3...v0.4.4
[0.4.3]: https://github.com/alforria-ai/alforria/compare/v0.4.2...v0.4.3
[0.4.2]: https://github.com/alforria-ai/alforria/compare/v0.4.1...v0.4.2
[0.4.1]: https://github.com/alforria-ai/alforria/compare/v0.4.0...v0.4.1
[0.4.0]: https://github.com/alforria-ai/alforria/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/alforria-ai/alforria/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/alforria-ai/alforria/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/alforria-ai/alforria/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/alforria-ai/alforria/releases/tag/v0.1.0

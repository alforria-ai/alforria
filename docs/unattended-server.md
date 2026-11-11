# Running alforria unattended

`alforria serve` can run as the engine of an always-on machine: bound to
localhost behind a reverse proxy, started by a supervisor, with nobody at a
terminal. This page is the reference for the settings and routes that exist
for that. All of them are alforria extensions; none is part of the upstream
opencode API, and none changes a frozen route or schema.

## Where these settings live: the host config

The keys on this page marked **host** are read once, when the server starts,
from the *host config*:

- the global config directory (`~/.config/alforria/opencode.json`,
  `opencode.jsonc` or `config.json`; see `crates/alforria-core/src/paths.rs`),
- `~/.opencode/`,
- the managed config directory (`/etc/opencode`).

A project's `opencode.json` and `.opencode/` directory are **not** consulted
for them. Those are files the agent can write; the server's access policy
must not be one of the things it can edit.

Unlike the per-project config, a host config file that does not parse — or
that exists and cannot be read, in the global config directory, `~/.opencode`
or the managed directory — is an error: `alforria serve` and `alforria web`
refuse to listen rather than run with a policy other than the one written
down. (A project file the server cannot read is skipped, as upstream.) Project config is treated as
restricted (see `server.trust`) for as long as the host config does not
load.

Which key is read from where is one table for every alforria key,
`EXTENSION_KEYS` in `crates/alforria-core/src/config/precedence.rs`; the
whole of it is in [assistant.md](assistant.md#which-config-may-set-what).
The loader applies it to each file as it is merged, so a host key written in
a project file, a remote config or one of the `OPENCODE_CONFIG*` environment
sources is dropped with a warning naming the file. Besides the keys on this
page, `server.resume` and `storage` (retention, `synchronous`) are host keys:
they decide what a restart does in every project and how durable the shared
database is.

alforria's own tables (`session_restriction`, `session_mark`, `session_ceiling`, `outbox`, and the trigger and
memory tables) are created by one set of migrations
(`crates/alforria-core/src/storage/alforria_migration.rs`), beside the
upstream schema and journal, which stay byte-compatible. A migration that
has shipped is never edited (the tests pin each body's hash); what one got
wrong is repaired by a later one, as `0008_session_mark_backfill` does for
databases written by earlier builds of this branch (see "Upgrading a
database" in `docs/assistant.md`).

Durations are strings with a unit (`"500ms"`, `"30s"`, `"5m"`, `"2h"`,
`"1d"`) or a bare number of milliseconds.

## Stopping: `server.shutdown` (host)

```jsonc
{
  "server": {
    "shutdown": {
      "timeout": "10s",     // the whole budget; default 10s
      "sessions": "abort"   // "abort" (default) or "drain"
    }
  }
}
```

On `SIGTERM` or `SIGINT` (ctrl-c elsewhere), `alforria serve` and
`alforria web`:

1. stop accepting connections;
2. settle the running turns. `abort` cancels them at once, exactly as
   `POST /session/{id}/abort` would, so each is recorded as interrupted.
   `drain` lets them finish and cancels whatever is still running shortly
   before the deadline;
3. kill the terminals and every process group a tool still has running (the
   bash tool, the session `shell` command and terminals each run their child
   in its own process group, so nothing is orphaned);
4. close the remaining connections. Event streams never end on their own and
   are not waited for;
5. checkpoint the SQLite write-ahead log into the database file
   (`wal_checkpoint(TRUNCATE)`);
6. exit 0.

A second signal skips the wait. Give the supervisor a kill timeout a little
longer than `timeout` (systemd `TimeoutStopSec=`, s6 `timeout-kill`).

A hard kill (`SIGKILL`, power loss) is safe for the database: SQLite replays
the write-ahead log on the next start. What a hard kill cannot do is stop the
tool processes or record the interrupted turn.

On the way down the trigger schedulers stop first — nothing fires any more,
by the clock, a watch, a webhook call or a request — and the transcript
indexers end. A trigger's turn that is already running is settled with the
others, aborted or drained as `sessions` says, and its run is recorded as
failed ("the turn was stopped before it finished") or completed. A turn
that ended inside the process leaves no restart marker, so the next start
repairs and resumes nothing on its account.

A trigger run that was still waiting for its turn (`triggers.concurrency`
runs go at a time) does not start once the schedulers have stopped: it is
recorded as skipped ("the server was shutting down before this run's turn
came"). If it answered a scheduled tick, the tick is given back, so the
next start fires it for a one-shot or a trigger with `catch_up`. And the
settling in step 2 is not one pass over the turns running at one moment:
a turn that reaches its session while the others are being settled — a
request already in flight, a trigger run that had passed its last check —
is settled too, until nothing is running and nothing is on its way, or
the deadline passes.

A hard kill between a scheduled tick being taken and its run starting no
longer loses the tick. The run is written in the transaction that takes
the tick; the next start (or the follower that takes over) finds it
queued under an owner that is gone, closes it as interrupted and gives
the tick back. A one-shot then fires, late, as any missed one-shot does
unless it says `catch_up: false`; a cron or interval trigger fires it
only with `catch_up`. A run whose turn had started is different: its tick
stays taken and the turn is `server.resume`'s to deal with.

### Two servers on one data directory: leader and follower

Nothing stops two alforria processes from sharing a data directory: a TUI
beside `alforria serve`, `serve` beside `web`, or a supervisor that starts a
second instance before the first is gone. Both serve requests. What must
happen once happens once, decided by three mechanisms, none of which a
process alone on its data directory can lose:

- **A lock per role.** For each project directory, one process is the
  *scheduler* (the leader): it runs the ticks, watches the paths and plans
  the next occurrence. The others are *followers* for that directory: they
  watch nothing and fire no tick, and say so once in the log
  (`another process schedules the triggers of …: this one is a follower`).
  The role is an advisory lock (`flock`) on
  `<data>/lock/scheduler-<hash of the directory>.lock`, taken by the first
  process that opens the directory with its scheduler on (`serve`, `web`,
  the TUI). The same goes for maintenance — event retention and write-ahead
  log checkpoints — under `<data>/lock/maintenance.lock`.
- **A claim per fire.** A trigger has one unsettled run across all the
  processes, enforced by a unique index: the insert that records a run as
  queued is the claim, and of two servers that fire the same trigger at the
  same moment one insert succeeds. This covers every fire — a scheduled
  tick (claimed together with its tick, in one transaction), a webhook
  call, a watch and a manual run. The fire that loses does not run: it is
  recorded as `skipped` ("the previous run has not finished"), exactly as
  when the run in the way is the same server's.
- **A claim per interrupted turn.** A restart marker whose process is gone
  is taken with an atomic rename before anything is done about it
  (`<data>/running/<session>.<pid>.<instance>.<boot>.claim`). Of the
  processes that start together after a crash, the one whose rename
  succeeds repairs the session and, under `server.resume: "continue"`,
  resumes the turn; the others never see it. Every process sweeps at
  startup, leader or not.

Each run and each marker records its **owner**: a random id the process
gave itself, its process id, the machine's boot id and, on Linux, the
process's start time. A starting server closes as interrupted only the
runs whose owner is provably gone — another boot of the machine, a process
id that no longer exists, or one that now belongs to a process started at
another moment — and leaves a live server's runs alone. (It used to close
every unsettled run in the directory, the other server's included.)

**A webhook call that reaches a follower** is run by the follower itself:
the call is claimed like any other fire, so it runs once whichever server
receives it, and nothing is forwarded between the processes. The same
holds for `POST /trigger/{id}/run`. A trigger created, changed, paused or
deleted through a follower is picked up by the scheduler on its next pass
(within 15 s).

**When the leader exits**, however it exits, the operating system drops its
locks. A follower asks for the scheduler's place again every 5 s (and for
the maintenance role every 5 minutes), so one of them takes over without a
restart. Taking over is a start as far as the schedule goes: the runs the
old leader left unfinished are closed as interrupted, and a tick that came
due in the gap is fired only by a trigger with `catch_up`. A clean
shutdown releases the scheduler's place as its first step.

Inside one process the scheduler's place goes to the newest instance of a
directory: when an instance is booted again (a config change disposes it),
the new one stops the scheduler of the one it replaces, whose running
turns finish, instead of waiting behind it as a follower.

Limits:

| | |
| --- | --- |
| The boot id | `/proc/sys/kernel/random/boot_id`, Linux only. Elsewhere there is none and a reboot is noticed by the process id being gone |
| Process-id reuse | Detected on Linux by the start time. On other Unix systems a dead owner whose id was given to another process reads as alive: its run stays `running`, and its trigger is skipped, until that process exits |
| Platforms that are not Unix | Whether a process is alive cannot be told: a starting server closes every unsettled run that is not its own (the old behaviour), and restart markers of other processes are left alone |
| Filesystems without `flock` (some network mounts) | Every process takes itself for the leader: ticks and other fires are still claimed in the database and run once, but each process watches the paths, so a watched change can run once per process if the first run has ended before the second server's debounce does |
| Webhook deliveries (`hook.webhook`) on a filesystem without `flock` | Every process takes itself for the one that delivers: a delivery can be sent once per process, under the same delivery id, and a hook's order is not kept between them. Receivers deduplicate on the id either way |
| The webhook rate limit | `triggers.webhook_min_interval` is counted per process: two servers accept one delivery each per interval. Only one runs while the first is unfinished |
| Different configs | Triggers declared in a config are scheduled by the leader from *its* config. Two servers meant to share a data directory should share the config |
| A tick taken by a process that died before running it | Given back when the run is found queued under an owner that is gone, which is at the next start or takeover. Until then the trigger shows the run as queued |
| A build older than this | Knows none of the above. Do not run it beside a newer one on the same data directory |

## What the agent's shell can see: `shell_env` (host)

```jsonc
{
  "shell_env": {
    "inherit": "all",                  // "all", "none", or ["PATH", "HOME", "LC_*"]; the default depends on the posture, below
    "allow":   ["GITHUB_TOKEN"],       // exact names released from the default denials (and added to the allowlist)
    "set":     { "TERM": "dumb" },     // set in every shell, over anything inherited
    "deny":    ["MY_SECRET_*"]         // never inherited; names or * / ? patterns, matched without regard to case
  }
}
```

The key is `shell_env` rather than `shell.env` because upstream's `shell` is
a string (the shell to run).

Every process the agent or a client drives starts from this policy instead
of the server's whole environment: the `bash` tool, the session `shell`
command, `` !`cmd` `` substitutions in command templates, terminals (`/pty`)
and worktree start scripts ("shells"), and the helpers the server starts —
MCP stdio servers, language servers and the commands that look for one,
and the server's own `git` calls.

### Two postures

The server runs the agent's processes in one of two postures.

| | **Standalone** | **Restricted** |
| --- | --- | --- |
| When | a developer's own server: bound to loopback, with none of the features on the right | the server is unattended or reachable from another machine (below) |
| A shell or helper starts from | the server's whole environment | an allowlist: `PATH`, `HOME`, `LANG`, `LC_*`, `TERM`, `TZ`, `USER`, `LOGNAME`, `SHELL`, `TMPDIR`, and the names in `allow` |
| Withheld from it | what is alforria's own to protect: the server's secrets and the model providers' key variables | that, and every name that looks like a credential |
| `GITHUB_TOKEN`, `GH_TOKEN`, `AWS_*`, `NPM_TOKEN`, `PGPASSWORD`, `SSH_AUTH_SOCK` | passed, as upstream | withheld unless `allow` names them |

**Standalone is upstream's behaviour with one exception**: alforria's own
secrets and provider API keys are not passed to the agent's processes. `gh`,
`aws`, `npm publish`, `psql` and an MCP server that reads a token from its
environment work as they did.

**Restricted applies when any of these holds** — one predicate,
`unattended_or_remote`:

- `server.auth.bearer` is configured (`keys` or `tokens_sha256`);
- the listener is bound to an address that is not loopback (`--hostname
  0.0.0.0`, a LAN address, a host name other than `localhost`);
- `permission.unattended` or `question.unattended` answers for nobody
  (an `on_timeout` other than `wait`);
- `hook.webhook` lists a webhook;
- triggers are enabled and a `webhook` or `watch` trigger exists, in the host
  config or stored;
- the host says so: `server.trust.project: "restricted"`.

Nobody is at the terminal of such a server to own the credentials in its
environment, and no list of secret-looking names is complete; so its
processes get a fixed, small environment and the host adds to it by name.
`allow` takes exact names, not patterns:
`"allow": ["HTTPS_PROXY", "NO_PROXY", "CARGO_HOME"]`.

This predicate decides the shell environment's posture and nothing else.
What a project's config may set and read (`server.trust.project`, restricted
by default with bearer auth only) and what a token may do (its scope) are
decided separately, as described further down.

The posture is logged once when the server starts, with its reasons, and an
operator can read it from `GET /server/posture` (Basic auth or an `admin`
token):

```jsonc
{
  "unattendedOrRemote": true,
  "reasons": ["bearer-auth", "non-loopback-bind"],  // at startup; also trust-project-restricted,
                                                     // permission-unattended, question-unattended,
                                                     // hook-webhook, webhook-or-watch-trigger
  "tightened": [],                                   // reasons that appeared since
  "shellEnv": { "posture": "restricted", "inherit": "default" }
}
```

**The posture only tightens while the server runs.** When a `webhook` or
`watch` trigger is created after startup — over HTTP, by the `schedule`
tool, or by a project config read when a directory is opened — or a listener
is bound beyond loopback, the server switches to the restricted posture from
the next process on, logs it once with the reason and lists it under
`tightened`. Deleting the trigger does not switch it back; a restart
re-evaluates. The other settings (`server.auth.bearer`,
`permission.unattended`, `question.unattended`, `hook.webhook`,
`server.trust`) are read when the server starts, like the rest of the host
config.

**A run that outside input started is always restricted.** A process started
for a session that carries the outside-start mark — a webhook or watch
trigger's run, its subagents and forks — or for a restricted session (one a
`chat` or `connector` credential created) gets the restricted environment
whatever the server's posture is at that moment. This covers the `bash`
tool, the session `shell` command and inline shell in command templates. It
does not cover helpers shared by a whole project (MCP servers, language
servers, `git`): those follow the server's posture.

### Choosing for yourself

`shell_env` overrides the posture's defaults in either direction:

- `inherit` set (`"all"`, `"none"` or a list) replaces the default selection
  for shells in either posture, still minus the denied names. On a restricted
  server helpers keep the allowlist in addition, so that `git` and a language
  server still find `PATH` and `HOME` under `"inherit": "none"`.
- `allow` releases an exact name from every default denial, in the case it
  is written in, and adds it to the allowlist. It is how a restricted server
  hands the agent a credential (`"allow": ["GITHUB_TOKEN", "AWS_PROFILE"]`),
  and how any server passes a provider key through.
- `deny` withholds names or patterns in either posture and is the one thing
  `allow` does not lift. A standalone host that wants the restricted denials
  writes them: `"deny": ["*_TOKEN", "*_KEY", "*_SECRET", "AWS_*"]`.
- `set` wins over everything, for shells.

A variable reaches a shell when, in order:

1. `set` names it (the configured value wins over everything); otherwise
2. no `deny` pattern matches it, and
3. it is not denied by default (below), unless `allow` names it or `inherit`
   is a list that names it exactly (a pattern such as `"*"` does not release
   a denied name), and
4. it is inherited: `inherit` selects it, or, when `inherit` is not set, the
   posture's default does.

### What is denied by default

Matched without regard to case (`openai_api_key` is `OPENAI_API_KEY`; so are
the patterns in `deny`).

In **both postures** — alforria's own:

- the server's own secrets and internal variables, and the model providers'
  and search backends' key variables (`BUILTIN_DENY` in
  `crates/alforria-core/src/shell_env.rs`): `OPENCODE_SERVER_PASSWORD`,
  `OPENCODE_SERVER_USERNAME`, `OPENCODE_AUTH_CONTENT`,
  `OPENCODE_CONFIG_CONTENT`, `OPENCODE_API_KEY`, `ALFORRIA_*_SECRET`,
  `ALFORRIA_*_TOKEN`, `ALFORRIA_*_KEY`, `ALFORRIA_*_PASSWORD` (the bearer and
  webhook secrets), `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GEMINI_API_KEY`,
  `OPENROUTER_API_KEY`, `LIBERTAI_API_KEY`, `EXA_API_KEY`, …;
- names shaped like an API key: `*_API_KEY` and `*_API_TOKEN`
  (`API_KEY_PATTERNS`) — a provider no list knows keeps its key in
  `MY_CORP_API_KEY`;
- every name in a provider's `env` list — from the model catalog, from
  `provider.*.env` in the host config and from the merged config of each
  project the server opens (a project can declare a provider of its own) —
  whatever it is called and whether or not it holds a secret (a provider
  that lists an account id or a region there has it withheld too);
- every variable a config reads through `{env:NAME}`: the host's files (the
  managed directory, the global config directory, `~/.opencode`) and each
  opened project's own config files.

`PATH`, `HOME` and the rest of the allowlist are never denied on account of
the last two.

The last two are *learned*: read from the host config and the catalog when
the server starts, and again every time a directory is opened and every time
the catalog is read. **A learned name stays withheld until the server
restarts** — closing the project, or removing the provider from its config,
does not release it, because the project's turns, terminals and MCP servers
can outlive both. A restart forgets the names no config or catalog mentions
any more; `allow` is how the host passes on one that is still mentioned
(like the rest of the host config it is read when the server starts).

On a **restricted** server only — names that look like a credential
(`CREDENTIAL_PATTERNS`, same file): `*_TOKEN`, `*_KEY`, `*_SECRET`,
`*_PASSWORD`, `*_PASSWD`, `*_PASSPHRASE`, `*_CREDENTIALS`, `*_CREDENTIAL`,
`*_PAT`, `*_ACCESS_KEY_ID`, `*_CONNECTION_STRING`, `AWS_*`, `AZURE_*`,
`GOOGLE_APPLICATION_CREDENTIALS`, `GOOGLE_CLOUD_KEYFILE_JSON`,
`CLOUDSDK_AUTH_*`, `PGPASSWORD`, `MYSQL_PWD`. With the default allowlist
these do not pass anyway; the patterns matter when the host sets `inherit`
itself.

### Helpers: MCP servers, language servers, git

MCP stdio servers, language servers, the commands that look for a language
server (`oxlint --help`, `java -version`, `xcrun --find`) and the server's
own `git` calls follow the posture too: everything except the denied names
on a standalone server, the allowlist on a restricted one. Their own
configured `environment` is added on top in either case. An MCP server
therefore gets one of the withheld variables only when its `environment`
names it, for example
`"environment": { "GITHUB_TOKEN": "{env:GITHUB_TOKEN}" }`, or when `allow`
releases it to everything. On a restricted server `git` fetches over SSH
only if `allow` names `SSH_AUTH_SOCK`.

Not covered, on purpose: programs the *user* runs at their own terminal or
desktop — the TUI's editor (`$EDITOR`), clipboard tool and browser opener,
the pager, `alforria db`'s `sqlite3`, `alforria pr`'s `gh` and `git`, the
command of `alforria providers login <url>`, and the desktop opener the
server uses for a sign-in URL. They are the user's own tools and get the
user's environment.

### Limits

- The denials go by name. On a standalone server a secret in any variable
  that is not alforria's own or a provider's — `GITHUB_TOKEN`,
  `DATABASE_URL`, `KUBECONFIG` — reaches the agent's processes, as it does
  upstream: add it to `deny`, or use a list for `inherit`. On a restricted
  server the default allowlist keeps it out.
- A provider key in a variable that is not shaped like an API key, of a
  provider that neither the catalog nor a loaded config lists, is not
  recognised.
- The `{env:NAME}` references of a remote config (`.well-known/opencode`)
  are not learned; its providers' `env` names are.
- Changing the posture's inputs in the host config takes a restart; only
  triggers and listeners tighten it while it runs, and nothing relaxes it.
  A project config that declares a `webhook` or `watch` trigger tightens the
  whole server once that project is opened.
- With `"inherit": "none"` a shell has no `PATH` or `HOME` unless `set`
  provides them.
- This is not a sandbox. A process running as the server's user can still
  read `/proc/<server pid>/environ` and the server's files (`auth.json`, the
  database). Keeping those from the agent takes running it as a different
  user, which alforria does not do for you, or secrets that never enter the
  server's environment.

## Who may connect: `server.auth.bearer` and `server.directories` (host)

HTTP Basic (`OPENCODE_SERVER_USERNAME` / `OPENCODE_SERVER_PASSWORD`) is
unchanged: it is the operator and may do anything. Next to it the server
accepts bearer credentials that say what their holder may do. The server
holds no signing secret for them: static tokens are stored as SHA-256
digests, and JWTs are verified against public keys.

```jsonc
{
  "server": {
    "auth": {
      "bearer": {
        // Ed25519 public keys of whoever mints the JWTs. PEM ("BEGIN PUBLIC
        // KEY"), or base64 / hex of the raw 32 bytes or of the SPKI DER.
        "keys": ["MCowBQYDK2VwAyEA…"],
        // The `aud` a JWT must carry: a string or a list. Required with keys.
        "audience": "agent-42",
        // Optional: the `iss` a JWT must carry.
        "issuer": "https://control-plane.example",
        // Clock skew tolerated on exp / nbf. Default 30s.
        "leeway": "30s",
        // Static tokens, as hex SHA-256 digests. A bare digest is `admin`.
        "tokens_sha256": [
          "9f86d081884c7d65…",
          { "sha256": "2c26b46b68ffc68f…", "scope": "connector", "name": "telegram-sidecar" }
        ],
        // Let tokens below `admin` answer a permission ask "always" (a
        // standing allow rule). Default false: "once" and "reject" only.
        "reply_always": false
      }
    },
    // The project directories a token narrower than `admin` may open,
    // matched exactly. Default: the server's working directory.
    "directories": ["/home/agent/workspace"]
  }
}
```

A client sends `Authorization: Bearer <token>` on every request, event
streams included (`?auth_token=` remains a Basic-only form).

### JWTs

- The algorithm is `EdDSA` (Ed25519). A token whose header names anything
  else (`none`, `HS256`, `RS256`, …) is refused before any key is used.
- `exp` is required. `nbf` is honoured when present. Both allow `leeway`.
- `aud` must contain one of the configured audiences.
- `iss` must equal `issuer` when `issuer` is configured.
- `scope` is a space-separated string or an array; it must name at least one
  of `chat`, `connector`, `admin`. The widest one named applies. `sub` is
  kept for logs.

A refused credential gets `401` with `WWW-Authenticate: Bearer
error="invalid_token"` and the reason in the body; a valid one that may not
do what it asked gets `403` (`ForbiddenError`). A request with a bearer
credential is never retried as Basic.

With `server.auth.bearer` configured and no `OPENCODE_SERVER_PASSWORD`,
`alforria serve` does not generate or print a password: Basic is simply off,
and requests without a token get `401` with `WWW-Authenticate: Bearer`.

### Scopes

`chat`, `connector` and `admin` each include the ones above them in this
table. `hook` stands apart: it opens one route, and `chat` does not include
it. **A route that is not listed needs `admin`**, so a route added later — a whole new family included
— is closed to narrower tokens until it is listed
(`crates/alforria-server/src/middleware/scope.rs`).

| Scope | For | Routes |
| --- | --- | --- |
| `hook` | Whatever calls **one** webhook trigger from outside | `POST /trigger/{id}/hook` for the trigger the credential is bound to, and nothing else — not even `GET /global/health`. The binding is the JWT's `trg` claim, or `trigger` on a static token (`{ "sha256": …, "scope": "hook", "trigger": "inbox" }`); a `hook` credential without one is refused everywhere, and with one it is refused (403, the same answer for every id) on any other trigger. If its own trigger is missing, paused or not a webhook trigger it gets one 404, whichever it is. `connector` and `admin` may call any webhook trigger (a `trg` on them narrows it the same way); `chat` may not. A credential has one role: `hook` beside `chat` in a token is `chat`. |
| `chat` | A person talking to the agent from a browser or a phone | `GET /global/health`, `GET /event`, `GET /attention`; sessions: `GET`/`POST /session`, `GET /session/status`, `GET`/`PATCH`/`DELETE /session/{id}`, `…/children`, `…/todo`, `…/diff`, `…/goal`, `GET`/`POST …/message`, `GET …/message/{id}`, `…/prompt_async`, `…/command`, `…/abort`, `…/fork`, `…/init`, `…/summarize`, `…/revert`, `…/unrevert`, `…/permissions/{id}`; replies: `GET /permission`, `POST /permission/{id}/reply`, `GET /question`, `POST /question/{id}/reply`, `…/reject`; files in the project: `GET /file`, `/file/content`, `/file/status`, `/find`, `/find/file`, `/find/symbol`, `POST /file/ticket`, `GET /file/raw`, `PUT /file/content`; what a client draws itself with: `GET /agent`, `/command`, `/skill`, `/path`, `/project/current`, `/vcs`, `/vcs/status`, `/vcs/diff`; its own schedules: `GET`/`POST /trigger`, `GET`/`PATCH`/`DELETE /trigger/{id}`, `POST /trigger/{id}/run`, `GET /trigger/{id}/runs`; memory: `GET`/`POST /memory`, `GET /memory/search`, `GET /memory/session`, `GET`/`PATCH`/`DELETE /memory/{id}`; and the durable replay of one session's events, `GET /api/session/{id}/event` (the one `/api` route below `admin`) |
| `connector` | A trusted sidecar on the same machine (a messaging bridge, an MCP host) | everything in `chat`, the webhook entry `POST /trigger/{id}/hook`, plus `GET /global/event` (every directory's events on one stream), `POST /log`, status reads (`GET /mcp`, `/lsp`, `/formatter`, `/experimental/tool`, `/experimental/tool/ids`, `/experimental/resource`, `/project`, `/project/{id}/directories`), and watching and stopping workflow runs (`GET /workflow`, `GET /workflow/run`, `GET /workflow/run/{id}`, `POST …/wait`, `POST …/abort`) |
| `admin` | The control plane, the operator | everything: config, provider credentials (`/auth`, `/provider`, `/config/providers`), MCP management and OAuth, terminals (`/pty`), the session `shell` command, saving and starting workflows, `/tui/*`, message and part edits and deletes, `/instance/dispose`, the `/global` routes, sync, sharing, `/experimental/*`, the v2 `/api` surface, the web UI |

### Threat model: what each scope can cause

The principle: **a `chat` token can converse with the agent and answer its
asks. It can never execute anything except through the model, under the
operator's permission policy, and it can never change that policy.**
`connector` is `chat` with a wider view and obeys the same principle. Only
`admin` (and Basic auth, which is the operator) executes directly or sets
policy.

How it is enforced, in three layers:

1. The scope table decides which routes a token reaches.
2. A route says where a prompt or command comes from (`Caller`,
   `crates/alforria-core/src/session/caller.rs`): the operator, a caller
   below `admin`, or the engine itself. The engine then judges the values it
   actually resolves, at the point it uses them: the agent the turn will run
   as, the parts as they will be stored, the file about to be read. There is
   no second spelling of a value for a check to look at. (Routes also refuse
   early what could never run, such as a hidden agent named on a new
   session; that is a courtesy, not the control.)
3. **The restriction stays with the session.** A session created by a caller
   below `admin` is restricted for its whole life, and so is every session
   descended from it: the subagent sessions the task tool makes, forks,
   workflow agents. A restricted session bounds *every* turn in it, whoever
   or whatever starts it — a later request from the operator included, and
   above all the turns nobody requests: a `/goal` round, a subagent's run, a
   resume, a trigger. The restriction is read from the session at the one
   place every turn passes through, so a turn source that knows nothing
   about callers is still bound.

An operator who wants to take a restricted session over lifts the
restriction first, with an `admin` credential:
`DELETE /session/{id}/restriction` (`GET` reports
`{ "restricted": true|false }`). Until then the operator's own prompts to
that session are refused the same things a `chat` token's are. That is
deliberate: a session is a conversation someone else shaped, and running it
with full rights should be a decision, not a side effect of who happened to
send the next message.

What a holder of a `chat` or `connector` token **can** cause:

- Whatever the agent does in the allowed project when prompted, to the
  extent the operator's `permission` config allows it or asks. If `bash` is
  `allow`, a prompt can get shell run: that is the operator's policy, not a
  bypass of it. Write the policy for the least trusted token holder.
- Answering the agent's permission asks `once` or `reject`, and its
  questions. An `ask` rule is therefore a decision delegated to whoever
  holds a `chat` token; a `deny` rule is not theirs to lift.
- Choosing among the agents a client's picker shows (not hidden, not
  subagent-only) and among the configured models. An agent's own
  `permission` block is part of the operator's policy: if one visible agent
  is broader than another, a token holder can pick it. Disable agents that
  should not be reachable.
- Running the configured custom commands (`/name`) that contain no inline
  shell, with arguments that are text.
- Undoing the agent's own edits (`revert` / `unrevert`), which restores
  project files from the snapshots its turns took; forking, summarising,
  renaming, archiving and deleting sessions; aborting a turn.
- Switching tools *off* for a session (`tools: { name: false }`).
- Reading and listing files inside the project, its git status and diff,
  and uploading files into the upload area.
- Spending the operator's model budget.

What it **cannot** cause, and what stops it:

| It cannot | Because |
| --- | --- |
| Run shell directly | `POST /session/{id}/shell`, `/pty`, `/tui/*` and workflow scripts are `admin` |
| Run a command that has inline shell | A command whose template contains `` !`…` `` is refused whole (403) below `admin`, whatever the `bash` permission says. No attempt is made to judge the inline shell: what the shell would run depends on the arguments |
| Inject shell through a command's arguments | For every caller, the operator included: inline shell is taken from the configured template only, and the arguments reach it as shell parameters (`$1`, `$ARGUMENTS`), never as part of the command line |
| Attach or mention a file through a command's arguments | Below `admin` the arguments are text. `@file` and `@agent` mentions are resolved in the operator's template only; a mention typed as an argument, quoted or not, stays text |
| Read a file outside the project through a prompt | Every `file:` part, whatever produced it, is checked where it is read: the path, with `..` and symlinks resolved, must be inside the project. Other schemes (`http(s):`, anything but `data:` and `file:`), bare paths and MCP resource parts are refused, so the server and the provider are never told to fetch a URL of the caller's choosing |
| Grant itself permissions | `permission` on session create and update is refused (403); a `tools` map with any `true` (which becomes an allow rule) is refused |
| Make a standing allow rule | `always` on a permission reply is refused unless `server.auth.bearer.reply_always` is `true` |
| Run as a hidden or subagent-only agent | The agent a turn resolves to — named in the request, stored on the session, configured on the command, or the default — is checked before the turn is stored, for requests and for the engine's own turns alike. Naming one on a new session is refused outright |
| Act in an operator's session, or borrow its rights for later turns | It acts only in restricted sessions; an operator's session it may read or fork (see "Whose session it is") |
| Outlive its limits through the engine | The session it created is restricted for life, with its children and forks; goal rounds, subagent runs and resumes in it are bound as its requests were |
| Skip the `task` permission ask, or start a subagent directly | `agent` and `subtask` parts are refused. An `@agent` in a template does not become an agent part for such a caller |
| Prompt a subagent's session | A session with a parent is driven by its parent's turn; prompting it directly is refused |
| Rewrite what the model reads as its history | Message deletes and part edits and deletes are `admin`. Message and part ids on a prompt are ignored (the server assigns them), so a prompt cannot name an existing message or part and replace it |
| Answer another project's asks | A reply looks its ask up by id across loaded projects only within `server.directories` |
| Open another project, or a subdirectory as a project | `server.directories` is matched exactly |
| Plant config, instructions, agents, commands, skills, workflows or git hooks | Uploads below `admin` go to the upload area only, and never to a name the loaders act on (see "Files for a browser") |
| Read or change config, credentials or MCP servers | Those routes are `admin` |

Refusals are `403` with a message, except on `POST …/prompt_async`, which
answers `204` before the engine has looked at the prompt: there the refusal
arrives as a `session.error` event and nothing is stored or run.

#### Whose session it is

A caller below `admin` acts only in restricted sessions — the ones such a
caller created, their subagent sessions and their forks. In a session the
operator created it may **read** (messages, todo, diff, goal, children) and
**fork**; every other request to it is refused with 403: prompts, commands
(`/goal` and its `pause` / `resume` / `clear` included), `init`, `summarize`,
`revert`, `abort`, rename, delete, and answering that session's permission
asks and questions.

The reason is the turns that follow. Anything started in a session can
schedule work the engine runs later — the next goal round, a continuation, a
subagent's result — and that work runs with the session's rights. In an
operator's session those are the operator's. So the rule is not "track who
caused each scheduled turn" but "a restricted caller cannot cause one there
at all": the effective restriction of any engine turn is then the stricter
of the session's and its causer's by construction, because the two are
never different. The engine enforces it where turns are made
(`Caller::may_act_on`, checked in `create_user_message` and at the top of
`command`, before `/goal` arms anything); the routes refuse the request
early.

At the HTTP layer there is one predicate (`confine::may_act_on`) and one
guard (`confine::guard_session_writes`), laid over the whole router rather
than written into handlers. For a credential below `admin` it refuses every
request that is not a `GET` on a route with a `{sessionID}` in it — v1, v2
and alforria's own — unless the session is one the caller may act on. The
only exception is listed by name (`POST /session/{id}/fork`), so a route
added later is refused on an operator's session without anyone remembering
it; a test walks the route tables and holds each to that. The guard runs
after routing, on the route that matched and the session id as the handler
reads it. It used to sit in front of routing and looked the session up
under the raw path segment: a percent-encoded id (`/session/ses%5F…`)
named no session there — so nothing was refused, and the request was
placed by its `?directory=` instead of by the session's own directory —
and then named the operator's session to the handler. The same lookup now
decodes the segment as the router does. A session that does not exist is
the handler's 404.

The same predicate decides which pending asks a credential below `admin`
is shown: `GET /attention`, and the frozen `GET /permission` and
`GET /question`, list it the ones it may answer and leave out an
operator's session's. The lists keep upstream's shape.

- **Forks** are the bridge: a fork made by a restricted caller is restricted
  whatever its source was (`restricted = source restricted || caller
  restricted`); the source is untouched.
- **The other direction** is allowed and bounded: the operator may prompt a
  restricted session, and is held to that session's restriction while doing
  so. Its prompt does not lift anything; rounds that follow stay restricted.
- **A goal** records who set it (`GoalState.startedBy`). Pausing, resuming
  or clearing it is acting in the session, under the same rule; a resumed
  goal's rounds are bound by the session, which cannot have become less
  restricted in between.
- **Reads across sessions.** A `chat` token can list and read every session
  of the directories it may open, the operator's included, and receives
  their events on `GET /event`. With one user per machine that is intended:
  cross-session read is acceptable, cross-session privilege gain is not.
  There is no per-token isolation between sessions today; a deployment that
  puts several mutually untrusting users on one server needs one, and
  should not rely on scopes for it.

#### Routes with two identifiers

Where a route names a session and something inside it, the inner object is
looked up first and must belong to the outer one before anything is done
with it; one that belongs elsewhere is answered like one that does not
exist. This holds for every caller.

| Route | Inner id | Checked against |
| --- | --- | --- |
| `POST /session/{id}/permissions/{permissionID}` | the pending ask | the ask's own session must be `{id}` (404); below `admin` that session must be restricted |
| `POST /permission/{id}/reply` | the pending ask | below `admin`: the ask's own session must be restricted and in an allowed project |
| `POST /question/{id}/reply`, `/reject` | the pending question | the same |
| `GET`, `DELETE /session/{id}/message/{messageID}` | the message | its session must be `{id}` (404) |
| `PATCH`, `DELETE /session/{id}/message/{messageID}/part/{partID}` | the message and the part | the message's session must be `{id}`, and the part must be that message's (404) |
| `POST /session/{id}/revert` | `messageID`, `partID` | the same |
| `POST /session/{id}/fork` | `messageID` | only messages of `{id}` are copied; a foreign id copies nothing of the other session |
| v2 `/api/session/{id}/permission/{requestID}/…`, `/question/{requestID}/…` | the pending request | the request's session must be `{id}` (`owned_request`, `owned_question`) |
| `GET /file/raw`, `/file/content` with `?ticket=` | the ticket | the ticket's directory and resolved file must be the request's. A ticket is a bearer capability for that one file: it is not bound to a session or to who asked for it, by design (an `<img>` sends nothing else) |
| `/pty/{ptyID}/…` | the terminal | terminals are kept per project directory; an id from another directory is not found. `admin` only |
| `/workflow/run/{runID}/…` | the run | runs are kept per project instance; an id from another directory is not found |

One shared step does the lookup-then-authorise for answers
(`confine::authorise_answer`) and one for messages and parts
(`require_owned`). Trigger and memory routes from other branches need the
same: fetch by the inner id, compare its directory or session with the
outer scope, then act.

#### Every field a `chat` token can send

| Request | Field | What the engine derives from it | Where it is judged |
| --- | --- | --- | --- |
| any | `?directory=`, `?location[directory]=`, `x-opencode-directory` | the project instance | one resolver (`middleware::directory`): the auth layer asks the allowlist about the value the route then acts in; every spelled directory must be a `server.directories` entry |
| any | `?workspace=` | a workspace | refused (auth layer) |
| any `/session/{id}/…` | the session id | the project (the session's own directory) | location layer, same allowlist |
| any `/session/{id}/…` except `GET` and `fork` | the session id | whose rights later engine turns run with | location layer (403 early) and core (`Caller::may_act_on`): the session must be restricted |
| `POST /session` | `title`, `metadata` | stored text | not restricted |
| | `parentID` | a child session | allowed; the child cannot then be prompted by this token |
| | `agent`, `model` | stored on the session | route: a hidden or subagent-only agent is refused; core: whatever agent a later turn resolves to is judged when it runs |
| | (the request itself) | the session's restriction | core: a session created below `admin` is restricted for life, with its children and forks |
| | `permission` | the session's ruleset | refused (route) |
| | `workspaceID` | a workspace | refused (route) |
| `PATCH /session/{id}` | `title`, `metadata`, `time.archived` | stored | not restricted |
| | `permission` | the session's ruleset | refused (route) |
| `POST …/message`, `…/prompt_async` | `parts[].text`, `system` | text for the model | not restricted: this is the conversation |
| | `parts[]` of type `file` | a file read, or inline data | core, at the read: `data:` or a `file:` inside the project |
| | `parts[]` of type `agent`, `subtask` | a turn that skips the `task` ask; a subagent run | refused (core) |
| | `parts[].id`, `messageID` | which stored row is written | ignored (core): ids are assigned |
| | `agent` | the agent of the turn | core, on the resolved agent |
| | `model`, `variant` | the model of the turn | any configured model; not restricted |
| | `tools` | session permission rules | core: `false` only |
| | `format`, `noReply`, `synthetic`, `ignored`, `metadata`, `time` | output shape and bookkeeping | not restricted |
| `POST …/command` | `command` | which template runs | core: refused when the template has inline shell |
| | `arguments` | text substituted into the template | core: text only, never scanned |
| | `agent`, and the command's configured `agent` | the agent of the turn | core, on the resolved agent |
| | `parts[]` (files) | file reads | core, at the read |
| | `model`, `variant`, `messageID` | as for a prompt | as for a prompt |
| `POST …/init`, `…/summarize` | `providerID`, `modelID`, `messageID`, `auto` | a model turn | as for a prompt |
| `POST …/fork` | `messageID` | a new session copied up to that point | the fork is restricted (core, `fork_as`) |
| `POST …/revert` | `messageID`, `partID` | a point in the session's own history | restricted sessions only; must belong to the session |
| `POST /permission/{id}/reply`, `…/permissions/{id}` | `reply` / `response` | the answer to an ask | route: `always` refused; the ask must belong to a restricted session in an allowed project |
| | `message` | feedback text for the model | not restricted |
| `POST /question/{id}/reply`, `/reject` | `answers` | the answer | the question must belong to a restricted session in an allowed project |
| `GET /file`, `/file/content`, `/file/raw`, `POST /file/ticket` | `path` | a file or directory read | route: inside the project, resolved as the kernel resolves the spelling (`protect::resolve_path`); the read is made on the resolved path |
| `PUT /file/content` | `path` | a file write | route: inside the upload area, not a loader name |
| `GET /find*`, `/vcs*` | `query`, `pattern`, `mode`, … | a search or diff of the project | no caller-chosen path |

The checks in routes are of two kinds. Those on requests that never become
a prompt (`permission` and `workspaceID` on session create and update,
`always` on a reply, file paths) each judge the very value the handler then
uses. The agent check on session create is early validation only; the
engine's check on the agent a turn runs as is the one that counts.

#### Turns the engine starts by itself

No request is being made when these run, so there is no caller to check.
Each is bound by the session it runs in (`Caller::Session`): restricted if
the session is.

| Turn source | Where its restriction comes from |
| --- | --- |
| `/goal` rounds after the first, and the goal reviewer's verdicts | the session. A caller below `admin` can only set, pause, resume or clear a goal in a restricted session, so the session's restriction is never weaker than the goal starter's. `GoalState.startedBy` records who set it. The round cap and stall limit are unchanged |
| A subagent run by the task tool | the child session, which inherits from its parent when it is created. Its agent is the one the model chose under the parent's `task` permission, so the "an agent a client could pick" rule does not apply to it; everything else does |
| A subagent's result delivered into its parent | the parent session |
| A command's configured subtask | the session; below `admin` a command configured to run as a subagent is refused |
| Workflow agents started by the `workflow` tool | child sessions of the session that called the tool: inherited. (Starting a workflow over HTTP is `admin`.) |
| A fork's notice to the model | the fork, which inherits from its source |
| Compaction, titles and summaries | side calls to the model, not turns: no agent of anyone's choosing, no tools, no files |
| Restart resume, triggers (other branches) | must pass `Caller::Session`, and create any session with `CreateInput::restricted` set from whoever the turn is for (a trigger made by a caller below `admin` runs in a restricted session, or is refused on an unrestricted one with `Caller::may_act_on`). A session created with a `parent_id` inherits without being asked |

`Caller` has no default value: every place that builds a prompt has to say
which of the three it is, or it does not compile.

#### What stays with the operator

- The `permission` config is the whole of what a token holder can make the
  agent do. A server whose config allows `bash` to every agent gives every
  `chat` token holder shell in the project, through the model. For an
  unattended agent that is usually the point; when it is not, set `bash`
  (and `edit`, `webfetch`, …) to `ask` or `deny` and decide who holds tokens
  that can answer.
- **The host's `permission` is a ceiling, not an earlier entry in a list.**
  Upstream evaluates one last-match-wins list in which a project's
  `agent.<name>.permission` comes after the host's rules — so a file in the
  project overruled a host or managed `deny`. alforria evaluates an agent's
  rules twice, as every config wrote them and as the host's own config alone
  has them (the managed directory, the global config directory, `~/.opencode`
  and `OPENCODE_CONFIG*`), and answers with the stricter: deny over ask over
  allow. A project's or a remote config can narrow a permission; it cannot
  widen one. (The managed directory is still merged last, which decides
  between two *host* files; it was never what stopped a project.) Rule lists
  the API shows are a flat rendering of this, never more permissive than it;
  the server decides by the policy (`tool::permission::Policy`).
- **On a server with bearer auth, project config is restricted.** The
  project directory is the agent's workspace: it can write `opencode.json`
  and `.opencode/`, and a clone brings its own. When `server.auth.bearer` is
  configured (or `server.trust.project` is `"restricted"`, or the host config
  cannot be read) a project's or a remote config keeps only keys that cannot
  carry a capability — see `server.trust` below. Everything else is dropped
  with a warning naming the file.
- **A restricted session, or one that outside input started (the *outside
  start* mark), does not write what the loaders read** — as defence in depth. Every file operation
  of `edit`, `write` and `apply_patch` (a patch's adds, updates, deletes,
  and both ends of a move), and the upload route, put the path they are
  about to modify to one guard (`alforria_core::protect`), at the write and
  on where the path really leads. It refuses a loader name anywhere on the
  path — `opencode.json`, `.opencode/`, `AGENTS.md`, `CLAUDE.md`, `.claude/`,
  `.agents/`, `.git/`, in the project, in an ancestor directory (the loaders
  walk up, to `/` for a directory that is not a repository) or in the home —
  and every location the loaders and the server consult for the instance:
  the global config directory, the managed directory, the data, state and
  cache directories, the binary's directory, and what the config names
  (`OPENCODE_CONFIG*`, `skills.paths`, `workflows.paths`, local
  `instructions`, globs included). The path is resolved the way the kernel
  resolves it; one that cannot be (a `..` after a directory that does not
  exist), one whose last component is a symlink, and one through a dangling
  link are refused outright in such a session: the guard does not guess
  where they lead. The write is then made on the resolved path that was
  judged — its parent directory is opened from the root one component at a
  time with no link followed, and the file is created, replaced or removed
  through that handle — so a rename or a link swapped in between the check
  and the write makes the write fail instead of redirecting it.
  The `edit_loader` permission in the host config hands back *project-level
  loader files*, per path (`"edit_loader": { "docs/AGENTS.md": "allow" }`, or
  `"allow"` for all of them) — a grant only a project wrote counts for
  nothing, and no rule lifts a host location (the data, state, cache, global
  and managed config directories, the binary's directory). The operator's
  own sessions are not guarded. The guard goes with the session, not with
  the server: a restricted or outside-started session continued from the
  command line (`alforria run --session`) is held to the same set, built by
  the same constructor (`Protected::for_loaded`). Session revert and
  unrevert are held to it too: in such a session one that would rewrite or
  delete a protected file is refused whole (`403`, naming the files) and
  nothing is restored; the other reverts of the session go through, and
  write exactly the files that were checked, through the same handle-based
  write as the tools. To
  revert past a change to a loader file, lift the restriction
  (`DELETE /session/{id}/restriction`, `admin`) or restore the file by hand.

  **This is not the control.** A session that may run `bash` writes any
  file the server's user can, and the guard does not see it. What keeps a project
  from carrying policy on an unattended server is the two points above —
  the host's permission ceiling, and project config restricted to keys that
  carry nothing. Deny `bash` (or run the agent as a user that cannot write
  these places) if the files themselves must be safe.

  What the write guard does not cover:

  | Not covered | Who can use it | What it means |
  | --- | --- | --- |
  | Writes by anything but `write`, `edit`, `apply_patch` and the upload route: `bash`, a formatter, an LSP or MCP server, a workflow script | The session itself, when its policy lets it run them | The guard never sees these. Deny `bash` and do not hand the session such helpers, or run the server as a user that cannot write the loader files |
  | A directory renamed after the write: the write lands in `plain/`, then `plain` is renamed `.opencode` | A process able to rename inside the project while the session runs — in practice the session's own shell (`bash` allowed) or another local process of the same user | No race is involved and no check at the write can see it. It needs exactly the capability of the row above, so it adds nothing to it. The window between the guard's check and its write is closed (the write goes through the judged directory's handle); names changing afterwards are not something a write-time guard decides |
  | The last instant of the check-to-write window on a platform without `openat` (Windows) | The same process, racing the write | There is no directory handle to write through. The write is made by name, with the judged path resolved again immediately before each step (creating directories, creating the file, the rename, a removal) and the step refused when the path has changed: a link swapped in before that comparison is refused, one swapped in between the comparison and the system call after it is followed. Unattended deployments are expected on Unix, where the window is closed |
  | Files a guarded session's revert does not restore | — | In a guarded session revert and unrevert judge every file they mean to write, refuse the whole revert (`403`, nothing written, the session not marked reverted) when one is a protected location, and then write exactly the judged files, each at its judged path through the directory handle — not `git checkout`, which writes by its own listing and by name. So the check and the write cannot disagree and the check-to-write window is closed as for a tool. It is narrower than an operator's revert: a file the snapshot listing leaves out (git-ignored files) and a path the snapshot holds as a symlink or a submodule stay as they are. No `edit_loader` grant is consulted: a revert has no agent |
  | Sessions without a guard: the operator's own (not restricted, not started by outside input) | — | By design. They write by name, exactly as upstream does |
- The host config (`server.*`, `shell_env`, `hook.*`) is never read from the
  project, whatever the agent writes there.

#### `server.trust` (host)

```jsonc
{
  "server": {
    "trust": {
      // "restricted" | "full". Default: "restricted" when server.auth.bearer
      // is configured, "full" otherwise.
      "project": "restricted",
      // Keys a restricted project config may set after all: dotted paths,
      // `*` for any one name.
      "project_keys": ["mcp", "agent.*.permission"],
      // Environment variables {env:NAME} (and ${NAME} in a base URL) may
      // read from a project config.
      "project_env": ["MY_PROJECT_REGION"],
      // "managed": host keys are read from the managed directory only.
      // Honoured only when the managed directory itself says it.
      "host": "managed"
    }
  }
}
```

| | Full (standalone default) | Restricted (bearer default) |
| --- | --- | --- |
| Open keys — `model`, `small_model`, `default_agent`, `username`, `logLevel`, `layout`, `snapshot`, `watcher`, `attachment`, `tool_output`, `disabled_providers`, `enabled_providers`, `command.*` (template without inline shell, description, agent, model, variant, subtask), `agent.*` (name, model, variant, temperature, top_p, prompt, description, mode, hidden, color, steps, maxSteps, disable), `provider.*` (name, id, whitelist, blacklist) and `provider.*.models.*` (id, name, family, release_date, attachment, reasoning, temperature, tool_call, interleaved, cost, limit, modalities, experimental, status), `compaction` (tail_turns, preserve_recent_tokens, reserved, default_context), `prompt`, `tool` | kept | kept |
| Capability keys — `permission`, `tools`, `agent.*.permission` / `.tools` / `.options`, `mode` rules, `mcp`, `lsp`, `formatter`, `plugin`, `shell`, `instructions`, `skills`, `workflows`, `references`, `provider.*.api` / `.npm` / `.env` / `.options`, `provider.*.models.*.provider` / `.options` / `.headers` / `.variants`, `enterprise`, `share`, `autoshare`, `autoupdate`, `subagent_depth`, `compaction.auto` / `.prune`, `experimental`, upstream `server.*`, a `command` with inline shell | kept (permission rules under the host's ceiling) | dropped unless listed in `project_keys` |
| A key no table names | kept | dropped |
| `{env:NAME}` in a project or remote config | substituted, as upstream; a name the shell environment policy withholds (or an `auth.json` credential) is warned about once per file; a name a project's own file refers to is withheld from the agent's processes from then on | only names in `project_env`; anything else is the empty string |
| `{file:path}` in a project or remote config | read, as upstream; an absolute, `~` or `..` path is warned about once per file | a relative path that stays inside the project, opened beneath it in one resolution; anything else is a config error. Never in a remote config |
| `${NAME}` in a provider base URL of a project or remote config | expanded once at load, every name, as before | expanded once at load under the `{env:}` rule; a refused name is a config error |
| `${NAME}` in any base URL at request time | only a URL the host's own config or the model catalog wrote is expanded | same |

The classification is `UPSTREAM_KEYS` (beside `EXTENSION_KEYS`) in
`crates/alforria-core/src/config/precedence.rs`; a schema key in neither
table fails `every_schema_key_is_classified`. Markdown agents, commands and
modes under a project's `.opencode/` go through the same tables as its JSON.

**Where the host config itself lives matters.** `~/.config/alforria`,
`~/.opencode` and the data directory are host locations; if a directory the
agent may write contains one (a project at `$HOME`, or `bash` allowed there),
the agent can write host config. For an unattended deployment put the policy
in the managed directory (`/etc/opencode`), owned by another user, and set
`"server": { "trust": { "host": "managed" } }` there: the user's files then
carry no host key at all.

With bearer auth configured the server **refuses to listen** when a directory
in `server.directories` (or the working directory, when the list is empty)
overlaps a protected location — contains it, is it, or lies inside it, with
symlinks resolved on both sides and whether or not the location exists yet:
the data, state and cache directories, the managed directory, the binary's
directory, and — unless `server.trust.host` is `"managed"` — the global
config directory and `~/.opencode`. The error names both paths.

### Turns nobody requests, and callers that are not people

Triggers, restart recovery and memory (see [assistant.md](assistant.md)) add
turns that no request stands behind, and one caller that is not a person at
all. Each has a defined caller; none runs with more than the stricter of who
set it up and the session it lands in.

| Source | Runs as | Bound by |
| --- | --- | --- |
| A trigger the operator wrote (config, an `admin` request, the `schedule` tool in an unrestricted session) | `Caller::Session` | The session it lands in: unrestricted in a session of its own, restricted if it names a restricted session. One the `schedule` tool made is also under its creator's bounds, stored for the session of its own it runs in; the only session it can name is the conversation that scheduled it (see "A trigger that names a session"). |
| A trigger a restricted caller created (a `chat`/`connector` request, or the `schedule` tool in a restricted session) | `Caller::Restricted` — that caller's request, replayed | Recorded on the trigger when it is created and never changed. Its runs happen in restricted sessions only: the sessions it creates are created restricted, a session it names must be this directory's and restricted, and the engine refuses the turn if the session's restriction has been lifted since. Its prompt is text: `@file` and `@agent` mentions are resolved only in a prompt the operator wrote. |
| A webhook call (`POST /trigger/{id}/hook`) | The trigger's caller, above | What it delivers is outside text: the run's session carries the outside-start mark (and with it outside-content), its shells get the restricted environment (`shell_env`: the allowlist, no `GITHUB_TOKEN` or `AWS_*` unless `allow` names them) whatever the server's posture, and its rules are the read-only ceiling **plus whatever the trigger's own `permission` block grants** — a webhook trigger whose block allows `bash` runs `bash` on a body its caller wrote, so who may call it matters. A `hook` credential calls the one trigger it is bound to; the body is capped at 256 KiB before it is read, and a trigger accepts one delivery per `triggers.webhook_min_interval` (default 1 s; sooner is a 429 with `Retry-After`), whoever sends it. |
| A restart follow-up (`server.resume: notify` / `continue`) | `Caller::Session` | The interrupted session's own restriction and rules. A trigger's interrupted turn is resumed like any other, once; its run is closed as interrupted and its tick is not fired again. With several servers on the data directory, the one that claims the dead process's marker does this and no other (see "Two servers on one data directory"). |
| The same fire reaching two servers on one data directory (a tick, a webhook call, a watch, a manual run) | The trigger's caller, above | One unsettled run per trigger across the processes, claimed in the database: one server runs it, the other records it as skipped. Only a run whose owner process is provably gone is closed by another server. |
| The memory flush before a compaction | No turn: one model request with one tool | The session's own `memory` permission (an `ask` is a refusal), its outside-content mark and its restriction, all recorded on what it writes. |
| A `/goal` round, a subagent's run, a subagent's result, an auto-resume | `Caller::Session` | The session, as before. |

#### A trigger that names a session

A trigger's `session` is `"new"`, `"reuse"`, or a session id (the `schedule`
tool's `current` is the id of the conversation that scheduled it). The rule
behind everything below: **the bounds of a run are carried by the session
it runs in** — stored for it by the server (`session_ceiling`,
`session_mark`), inherited by its subagents and forks, and read wherever a
policy is assembled (`SessionStore::policy`). Nothing binds a run from
beside its session: a bound held only while the run's turn lasts would end
with the turn while a background subagent or workflow it started carried
on, and would never reach a subagent session that already existed.

So a trigger whose run needs bounds of its own always runs in a session of
its own, and a named session is only ever one whose own policy and marks
are already the right ones for the run:

| Trigger | May name | Why |
| --- | --- | --- |
| A `webhook` or `watch` trigger, or any run that carries a payload | no session, ever | The read-only ceiling and the outside-start mark belong on a session made for the run. |
| One a session's `schedule` tool made (it has creator's bounds) | only that very session — `current` — and only for `cron`, `interval`, `at`, `in` | The run is a turn of the conversation that asked for it, under that conversation's policy and marks as they then stand: the creator *is* the session, so there are no separate bounds to lose. Any other session would need the creator's bounds held over it. |
| One written after outside content was seen | only a session that already has the outside-content mark | With `current` that is the creating session itself. A session that has seen outside content cannot change a trigger that runs in a conversation that has not. |
| One the operator wrote (the config, an `admin` request) | any session of this directory | It has no bounds of its own; the run is a turn under that session's policy. Its `permission` block is not applied there — the block becomes the rules of a session the trigger makes. |
| One a `chat` or `connector` request created | a restricted session of this directory | `Caller::may_act_on`: it cannot schedule into an operator's session. |

The rule is one function (`trigger::rules::check_named_session`), asked
when a trigger is created, when its `session` or its kind is changed, and
again when each run starts. A trigger stored by an earlier build, one
whose target lost its restriction, or a run that arrives with a payload is
refused at the run: it is recorded as `failed` with the reason and nothing
is written to the session.

Three things can be recorded about a session. They are independent, each
has one cause and one effect, and all three live in tables only the server
writes (`session_restriction`, `session_mark`) — not in session metadata,
which a client can write:

| Mark | Cause | Effect |
| --- | --- | --- |
| **restricted** | A caller below `admin` created the session (this page). | Who may drive it, and what its turns may do. |
| **outside start** | A `webhook` or `watch` trigger started a run in it, or a trigger run delivered a payload. | The read-only ceiling: the rules the run was given are a ceiling nothing widens. The project's standing `always` approvals do not apply, an `ask` is a deny at once, and `permission.unattended` never answers for it — `on_timeout: "allow"` included. It cannot change existing memory records or write loader files. A fork keeps its rules along with its text. |
| **outside content** | A tool that returns somebody else's content answered in the session (`webfetch`, `websearch`, any MCP tool, a read from the upload area or a search that returned a file of it, an unverified memory record, an excerpt of a marked session, a marked subagent's result — returned by the `task` tool or delivered later as a message when a background subagent finishes), a caller below `admin` attached a file to a prompt, or the run belongs to a trigger a marked session wrote. Always set together with *outside start*. | Provenance: memory it writes is unverified and not injected into later sessions; of existing memory it may change or delete only unverified records it saved itself; triggers it schedules carry the mark; its transcript excerpts are fenced. **No tool's permission changes**: an operator's interactive session keeps working as before after a web search. |

The full list of causes, and the test that keeps the tool list complete, is
in [assistant.md](assistant.md#two-session-marks-what-causes-each-and-what-each-does).

All three are inherited by children and forks at any depth, and none is
cleared below `admin` (lifting a restriction is
`DELETE /session/{id}/restriction`; the other two are never removed by the
engine). `GET /session/{id}/restriction` reports all three. A client cannot
set them: `untrustedInput` and `triggerID` are stripped from metadata a
client supplies on session create and update.

What the outside-content mark does **not** claim: that a session without it
has read nothing hostile. `bash` output and ordinary project files are not
tracked (see assistant.md for why), so a prompt injection that arrives
through `curl` in a shell, or through a file in a cloned repository, can
still be saved as verified memory by a session that is allowed to write
memory. The mark closes the paths an outsider reaches *without* the
operator's shell: the web tools, MCP servers, uploads, webhook bodies and
what earlier marked sessions left behind.

A lookup that fails never reads as the permissive answer: a session whose
marks cannot be read counts as carrying both, a restriction that cannot
be read is an error or counts as one, and a session a trigger cannot verify
as its own does not exist for it.

### What a token narrower than `admin` is confined to

- **Directories.** The directory a request resolves to must be exactly one
  of `server.directories`. There is one resolver for it
  (`middleware::directory`), and the allowlist is asked about the value it
  returns — the same value, decoded the same way, that the route then acts
  in:
  - a v1 route reads `?directory=`, else the `x-opencode-directory` header;
  - a v2 route (`/api/…`) reads `?location[directory]=`, else the header;
  - alforria's own routes (`/trigger`, `/memory`, `/workflow`, `/file/*`,
    `/session/{id}/goal`) read `?directory=`, else `?location[directory]=`,
    else the header — and fall back to the server's working directory when
    none is given, which must then be an allowed directory itself;
  - a session in the path decides for its own routes, and is checked where
    it is resolved.

  Besides, a directory spelled under *any* of the three spellings must be
  allowed, also where the route does not read that spelling: an allowed
  directory under one never covers for another. A directory is judged
  where the kernel would land it: symlinks are followed where they stand,
  `..` is applied to where the path has got to, and a listed directory that
  does not exist yet behind a symlinked parent is where the link leads (the
  list is resolved when the server starts; a link repointed afterwards does
  not carry its entry along). Upstream's own reading, which a handler still
  uses to name the instance, folds `..` out of the spelling first; a
  spelling the two readings disagree about (`allowed/link/..`), or that
  cannot be resolved, is refused. A directory *below* a listed one is not allowed: opened
  as a project of its own it would load config from inside the project.
  Selecting a workspace is refused. When bearer authentication is
  configured and the server's working directory is not in
  `server.directories`, `serve` says so at startup: tokens below `admin`
  cannot use that directory, and must name one.
- **Files.** Reads and listings stay inside the project; uploads go to the
  upload area (see "Files for a browser").
- **Schedules.** It sets what the `schedule` tool can set: never a trigger's
  `permission`, `agent` or `model`. It changes, fires and deletes only
  triggers a restricted caller created; the config's and the operator's it
  can read. A session it names must be this directory's and restricted; at
  most 50 triggers exist that an agent or a restricted caller made.
- **Memory.** A write needs the default agent's `memory` permission for the
  action to be `allow` (an `ask` is a refusal: nobody can be asked for an
  HTTP call), and `GET /memory/session` needs `session_search`. With
  `?session=<id>` — a session of this directory it may act in — the rules
  are that session's policy instead, its ceilings included. What it
  writes is a client's record: shown to restricted sessions, never injected
  into the operator's, and its rewrite never verifies a record. Memory is
  kept per project, and for a directory that is not a git repository — all
  of which belong to the one `global` project — per directory: a token
  confined to such a directory reads and changes that directory's records
  and searches that directory's sessions, nothing of another's. The
  exception is yours to make: `memory.scope: "global"` shares one memory
  across everything, for every caller. The `/memory/unattributed` routes
  need `admin`. See "Whose records" in [assistant.md](assistant.md).
- **Event replay.** `GET /api/session/{id}/event` answers for sessions in an
  allowed directory, the operator's included (as `GET
  /session/{id}/message` does).

Basic auth, `admin` tokens and a server with no authentication are not
confined.

## When nobody answers: `permission.unattended`, `question.unattended` (host)

A permission ask and a question both wait for a person. With no client
connected, a turn that asks waits forever. These settings bound the wait.
They are read from the host config only: a project file cannot turn asks
into allows.

```jsonc
{
  "permission": {
    "bash": "ask",
    "unattended": {
      "timeout": "5m",        // how long an ask waits for a person
      "on_timeout": "deny"    // "wait" (default), "deny" or "allow"
    }
  },
  "question": {
    "unattended": {
      "timeout": "5m",
      "on_timeout": "reject"  // "wait" (default) or "reject"
    }
  }
}
```

`timeout` is required unless `on_timeout` is `wait`. `wait` is today's
behaviour and the default. (`permission.unattended` is this policy only when
it is an object with `timeout` or `on_timeout`; an action such as
`"unattended": "allow"` is still a rule for a permission of that name.)

- **`deny`** resolves the ask exactly as a person's *reject with a message*
  does: the tool call fails with "No one was available to answer this
  permission request within 5m, so it was denied automatically…", the model
  reads that and the turn continues. (A bare reject would stop the turn and
  leave the session waiting for the person who is not there.)
- **`allow`** resolves it as `once`: this call goes through, and nothing is
  approved for next time.
- A question that times out fails its tool call with "The user is not
  available: nobody answered within 5m. Do not ask again. Proceed on your
  best judgment where that is safe, and stop where the answer is needed."
  The turn continues.

Each ask has its own deadline. A timeout resolves that one ask; it does not
sweep the session's other pending asks along the way a person's reject does.
An answer that arrives in time wins as usual.

The usual events are published, with one added property so a client can tell
nobody answered: `permission.replied` carries `"automatic": true` (with
`reply: "reject"` or `"once"`), and `question.rejected` carries
`"automatic": true`. A person's reply is unchanged, byte for byte.

**Scheduled turns.** A trigger's run has nobody attached, so its asks and
questions follow these two policies and nothing else. With the default,
`wait`, a run that asks waits until someone answers (`GET /attention` lists
it, a webhook on `permission.asked` announces it), and its trigger skips
every tick until then: set a `timeout` on a server that runs triggers. A run
that carries outside text (a webhook or watch trigger) never gets as far as
the policy: for it an `ask` is a deny at once, so `on_timeout: "allow"`
cannot answer for text that came from outside.

### `GET /attention`

Everything that is waiting for a person, across every loaded project —
state, not events, so it is there for a client that was not connected when
the ask was made. Scope `chat`; a token below `admin` sees the asks it could
answer and nothing else: those in the projects it may open
(`server.directories`) and, there, in the sessions it may act in. An ask
waiting in an operator's session is not listed for it, in whatever
directory.

```jsonc
{
  "count": 1,
  "items": [{
    "kind": "permission",              // or "question"
    "id": "per_…",                     // what POST /permission/{id}/reply names
    "sessionID": "ses_…",
    "title": "Deploy the site",        // the session's title
    "directory": "/home/agent/workspace",
    "summary": "bash: git push",       // the command or first pattern; a question's text
    "asked": 1794408349584,            // ms since the epoch
    "age": 5321,                       // ms
    "expires": 1794408649584           // when the unattended policy answers; null if it waits
  }]
}
```

Oldest first. An item leaves the list when it is answered, times out, or
its turn is aborted. On a server with `hook.webhook` configured the
operator's answer (no authentication, Basic, or an `admin` token) has one
more member, `outbox`, described with the hooks below.

## Telling a control plane: `hook.webhook` (host)

`permission.asked`, `question.asked` and `session.idle` are bus events: they
reach whoever is connected at that moment. A control plane that must learn
an agent is waiting, with no client attached and across restarts on either
side, needs them delivered. Selected events are written to a durable outbox
and delivered until the receiver accepts them.

```jsonc
{
  "hook": {
    "webhook": [
      {
        "name": "control-plane",              // optional; defaults to the url
        "url": "https://api.example/agents/42/events",
        // Event types, by name or * / ? pattern.
        "events": ["permission.asked", "question.asked", "session.idle", "session.error", "trigger.*"],
        // Exactly one of:
        "secret": "{env:ALFORRIA_WEBHOOK_SECRET}",   // HMAC-SHA256
        "signing_key": "<base64 or hex of a 32-byte Ed25519 seed>"
      }
    ],
    "outbox": {                 // all optional; defaults shown
      "retry_initial": "5s",    // first retry delay, doubling…
      "retry_max": "5m",        // …up to this
      "max_age": "72h",         // an undelivered event is abandoned after this
      "retention": "24h",       // delivered and abandoned rows are kept this long
      "timeout": "10s",         // per attempt
      "max_pending": 10000      // undelivered rows kept; the oldest are dropped beyond it
    }
  }
}
```

Any event type on the bus can be selected; the names are matched as
strings, so events other subsystems publish (`trigger.fired`,
`trigger.completed`) need no change here. A hook without `secret` or
`signing_key` is a config error: deliveries are always signed.

**The request.** `POST <url>`, `Content-Type: application/json`, with:

| Header | |
| --- | --- |
| `X-Alforria-Delivery` | the delivery id (`whd_…`): the same on every attempt of this delivery. Deduplicate on it |
| `X-Alforria-Event` | the event type |
| `X-Alforria-Timestamp` | when this attempt was signed, unix seconds |
| `X-Alforria-Attempt` | 1, 2, … |
| `X-Alforria-Signature` | `t=<timestamp>,v1=<hex>` — HMAC-SHA256 of `"<timestamp>.<body>"` with `secret`; or `t=<timestamp>,ed25519=<base64>` — an Ed25519 signature of the same bytes with `signing_key` |

Verify the signature over the raw body, and reject a timestamp too far from
now to stop replays. With `signing_key` the receiver holds only the public
key and can forge nothing.

**The body** is small and the same on every attempt:

```jsonc
{
  "id": "whd_01JC…",                       // the delivery id again
  "type": "permission.asked",
  "event": "evt_…",                        // the bus event's id
  "time": 1794408349584,                   // when it was recorded, ms
  "directory": "/home/agent/workspace",
  "sessionID": "ses_…",                    // when the event has one
  "title": "Nightly deploy",               // that session's title
  "kind": "permission",                    // "permission" | "question" | null
  "properties": { "id": "per_…", "sessionID": "ses_…", "permission": "bash" }
}
```

`properties` holds only identifiers (`id`, `…ID`), short labels
(`permission`, `kind`, `name`, `status`, `reason`, `trigger`, `report`,
`action`), numbers and booleans from the event, and for `session.error` the
error's name. Message text, tool input and output, patterns and metadata
never leave the machine.

Every event can be selected by name or pattern, the assistant features'
included: `trigger.fired`, `trigger.completed`, `trigger.failed`,
`trigger.skipped` (`"trigger.*"`), `session.interrupted`,
`session.media.omitted` and `memory.updated`. A `permission.asked` raised
inside a trigger's run is delivered like any other, which is how someone
finds out a scheduled turn is waiting. `trigger.completed` carries the
trigger, run and session ids, `duration`, `quiet` and `report`; a
`trigger.failed` carries no error text. The one piece of content a delivery
can carry is a run's final answer, and only when the hook asks for it by
name:

```jsonc
{ "name": "bridge", "url": "…", "secret": "…",
  "events": ["trigger.completed"], "trigger_text": true }
```

adds `properties.text` (cut to 16 KiB) to `trigger.completed` when the run's
`report` is `"final"`. Without it the receiver reads the answer from
`GET /trigger/{id}/runs` with a token.
To act on a `permission.asked`, call `GET /attention` or `GET /permission`
with a token.

**Delivery.** A 2xx is success. What happens otherwise depends on the
answer:

| The receiver… | |
| --- | --- |
| answers 2xx | delivered |
| answers 4xx, other than 408, 425 and 429 | **given up at once, never retried**: the request itself was refused (a wrong URL, a rejected signature, a body it will not take) and sending it again changes nothing. The row is closed as failed with the status (`the receiver answered 404`) and a warning is logged |
| answers 408, 425, 429, any 5xx, a 3xx (redirects are not followed), or anything else | retried |
| does not answer within `timeout`, or cannot be reached | retried |

A retry comes after `retry_initial`, doubling up to `retry_max`, until the
delivery succeeds or the event is `max_age` old. Delivery is at least once:
a receiver that accepted a request whose response was lost sees it again,
with the same delivery id.

**Each hook is a queue of its own.** A hook's deliveries go out in the order
they were recorded, one at a time; while one is being retried, the ones
recorded after it wait behind it, and follow in order once it is delivered,
given up or `max_age` old. The hooks are worked at the same time: a
receiver that hangs or keeps failing delays its own deliveries and no other
hook's. (A receiver that must not lose its place behind one event it cannot
take should refuse that event with a 4xx.)

**Recorded before anything else happens.** The row is written while the
event is being published, before the publish returns: a server killed
right after a turn's `session.idle` has the delivery in its database, and
sends it when it starts again. The outbox is a table in the server's
database (`outbox`), so a server that stops while a delivery is backing
off resumes it too. Stopping the server never waits for a receiver: an
attempt in flight is dropped and made again on the next start, under the
same delivery id. Each hook owns its deliveries by `name`; removing or
renaming a hook abandons what it was still owed.

**Bounded, and it says so.** Beyond `max_pending` undelivered events the
oldest are deleted: those deliveries are never made. Each time it happens
the server logs a warning (at most one a minute, with the running total),
and `GET /attention` carries the numbers on a server that has hooks, for
the operator (no authentication, Basic, or an `admin` token) — they count
every directory's events, so a `chat` or `connector` token is not given
them:

```jsonc
{ "count": 0, "items": [],
  "outbox": {
    "pending": 12,   // still owed
    "failed": 1,     // given up (refused, too old, hook removed) and still kept by `retention`
    "dropped": 0     // deleted beyond max_pending since this server started
  } }
```

Without `hook.webhook`, or for a token below `admin`, the `outbox` member
is absent.

**Several servers on one data directory.** Every server records the
deliveries its own events owe, in the shared table; one of them sends. The
role is an advisory lock on `<data>/lock/outbox.lock`, like the scheduler's
and the maintenance role: the others ask for it every few seconds and one
has it when the holder stops or dies, so a delivery is not sent by two
servers at once. A row another server recorded is picked up within about
five seconds. `dropped` is counted per process.

## Files for a browser: tickets, raw reads, uploads

The frozen file routes return JSON and need an `Authorization` header. A
browser showing a workspace image in an `<img>`, or offering a download
link, can send neither a header nor parse JSON; and nothing could put a file
*into* the workspace. Three routes outside the frozen tables cover that,
plus one addition to a frozen route. All take `?directory=` (or the
`x-opencode-directory` header) like the rest of the v1 surface, and need the
`chat` scope.

Every `path` is relative to the project directory (an absolute path is
accepted when it is inside). It is resolved the way the kernel resolves it
— symlinks followed where they stand, `..` applied to where the path has
got to, so `link/../x` is beside where `link` leads and not `x`; a path with
no such reading (a `..` after a directory that does not exist, a link loop)
is refused — and the result must be inside the project directory or its git worktree —
the boundary the tools draw with `external_directory`. A tool can ask the
user to cross it; an HTTP request cannot, so it gets `403`, whoever sends
it.

### `POST /file/ticket`

```jsonc
// request
{ "path": "out/chart.png", "ttl": 60 }   // ttl in seconds: default 60, at most 600
// 200
{
  "ticket": "5b0c1f0e-…",
  "expires_in": 60,
  "path": "out/chart.png",
  "url": "/file/raw?path=out/chart.png&directory=%2Fhome%2Fagent%2Fworkspace&ticket=5b0c1f0e-…"
}
```

The ticket reads that one existing file of that one project, and nothing
else: another path, another directory, another route or a write with it gets
`401`. It is not consumed by use (a browser fetches an embedded image more
than once); it stops working when it expires. `404` when the file does not
exist, `400` when it is a directory.

### `GET /file/raw?path=…[&ticket=…][&download=1]`

The file's bytes, with its media type, `Content-Length`, single-range
`Range` support (`206`, `416`), and `Content-Disposition: inline` (or
`attachment` with `download=1`). Authenticated by a ticket in the URL or by
the usual header.

The response is `nosniff`, `Cache-Control: private, no-store`, and carries
`Content-Security-Policy: sandbox`: an HTML or SVG file the agent wrote is
shown as an inert document, not run as a page of the server's origin.

The frozen `GET /file/content?path=…` (JSON) accepts `&ticket=…` the same
way.

### `PUT /file/content?path=…[&overwrite=false]`

The request body is the file, byte for byte (no multipart, no JSON
envelope). Missing directories are created. The body is streamed to a
temporary file beside the target and renamed over it, so a failed or
oversized upload leaves the old file as it was.

```jsonc
// 201 when created, 200 when replaced
{ "path": "inbox/report.pdf", "absolute": "/home/agent/workspace/inbox/report.pdf", "size": 48213, "created": true }
```

- `413` when the body exceeds `server.upload.max_bytes` (host config,
  default 32 MiB = 33554432), declared or not.
- `409` with `overwrite=false` when the file exists.
- `400` when the path is a directory; `403` when it leaves the project.

Below `admin`, an upload is further limited, because it is a write nobody
asked the model for:

- it must land inside an upload directory: `server.upload.directories`,
  relative to the project, default `["uploads"]`. Everything else in the
  project needs `admin`. This is an allowlist on purpose: "anywhere except
  the files that matter" is a list that falls behind. `["."]` opens the
  whole project to uploads, for deployments that want it;
- in either mode, no component of the path may be a name the loaders act
  on, in any case and at any depth, existing or to be created:
  `opencode.json`, `opencode.jsonc`, `config.json`, `AGENTS.md`,
  `CLAUDE.md`, `CONTEXT.md`, `.opencode/`, `.claude/`, `.agents/`, `.git/`,
  `.alforria/`. The list is `alforria_core::project_files`, the constants
  the loaders themselves discover by, so a discovery location added there
  is closed to uploads at the same time;
- both rules apply to the resolved path, so a symlink inside the upload
  area does not lead out of it or into a loader directory.

A `file.edited` event is published for the written file.

```jsonc
{ "server": { "upload": { "max_bytes": 33554432, "directories": ["uploads"] } } }
```

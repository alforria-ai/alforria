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

Unlike the per-project config, a host config file that does not parse is an
error: `alforria serve` and `alforria web` refuse to listen rather than run
with a policy other than the one written down.

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

## What the agent's shell can see: `shell_env` (host)

```jsonc
{
  "shell_env": {
    "inherit": "all",                  // "all" (default), "none", or ["PATH", "HOME", "LC_*"]
    "allow":   ["OPENAI_API_KEY"],     // exact names exempt from the built-in deny list
    "set":     { "TERM": "dumb" },     // set in every shell, over anything inherited
    "deny":    ["MY_SECRET_*"]         // never inherited; names or * / ? patterns
  }
}
```

The key is `shell_env` rather than `shell.env` because upstream's `shell` is
a string (the shell to run).

Every shell the agent or a client drives starts from this policy instead of
the server's whole environment: the `bash` tool, the session `shell` command,
`` !`cmd` `` substitutions in command templates, terminals (`/pty`) and
worktree start scripts.

A variable reaches a shell when, in order:

1. `set` names it (the configured value wins over everything); otherwise
2. no `deny` pattern matches it, and
3. it is not on the built-in deny list, unless `allow` names it or `inherit`
   is a list that names it exactly (a pattern such as `"*"` does not release a
   built-in denied name), and
4. `inherit` selects it.

The built-in deny list (`BUILTIN_DENY` in
`crates/alforria-core/src/shell_env.rs`) holds the server's own secrets
(`OPENCODE_SERVER_PASSWORD`, `OPENCODE_SERVER_USERNAME`,
`OPENCODE_AUTH_CONTENT`, `OPENCODE_CONFIG_CONTENT`, `OPENCODE_API_KEY`,
`ALFORRIA_*_SECRET`, `ALFORRIA_*_TOKEN`, `ALFORRIA_*_KEY`,
`ALFORRIA_*_PASSWORD`) and the variables model providers and search backends
read their keys from (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`,
`GEMINI_API_KEY`, `OPENROUTER_API_KEY`, `LIBERTAI_API_KEY`, `EXA_API_KEY`, …).
General-purpose credentials such as `AWS_*` or `GITHUB_TOKEN` are not on it:
the agent's own tools need them, and whether it should have them is yours to
decide with `deny`.

MCP stdio servers and language servers are started with everything except
the denied names, then their own configured `environment`. An MCP server
therefore gets a provider key only when its `environment` names it, for
example `"environment": { "OPENAI_API_KEY": "{env:OPENAI_API_KEY}" }`.

With `"inherit": "none"` a shell has no `PATH` or `HOME` unless `set`
provides them; a list such as `["PATH", "HOME", "LANG", "LC_*", "TERM",
"USER", "SHELL", "TMPDIR"]` is usually what is wanted.

This is not a sandbox. A shell running as the server's user can still read
`/proc/<server pid>/environ` and the server's files (`auth.json`, the
database). Keeping those from the agent takes a separate user for its shell,
or secrets that never enter the server's environment.

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
    // The project directories a token narrower than `admin` may open.
    // Default: the server's working directory.
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

Each scope includes the ones above it in this table. **A route that is not
listed needs `admin`**, so a route added later — a whole new family included
— is closed to narrower tokens until it is listed
(`crates/alforria-server/src/middleware/scope.rs`).

| Scope | For | Routes |
| --- | --- | --- |
| `chat` | A person talking to the agent from a browser or a phone | `GET /global/health`, `GET /event`; sessions: `GET`/`POST /session`, `GET /session/status`, `GET`/`PATCH`/`DELETE /session/{id}`, `…/children`, `…/todo`, `…/diff`, `…/goal`, `GET`/`POST …/message`, `GET …/message/{id}`, `…/prompt_async`, `…/command`, `…/abort`, `…/fork`, `…/init`, `…/summarize`, `…/revert`, `…/unrevert`, `…/permissions/{id}`; replies: `GET /permission`, `POST /permission/{id}/reply`, `GET /question`, `POST /question/{id}/reply`, `…/reject`; files in the project: `GET /file`, `/file/content`, `/file/status`, `/find`, `/find/file`, `/find/symbol`; and what a client draws itself with: `GET /agent`, `/command`, `/skill`, `/path`, `/project/current`, `/vcs`, `/vcs/status`, `/vcs/diff` |
| `connector` | A trusted sidecar on the same machine (a messaging bridge, a scheduler) | everything in `chat`, plus `GET /global/event` (every directory's events on one stream), `POST /log`, status reads (`GET /mcp`, `/lsp`, `/formatter`, `/experimental/tool`, `/experimental/tool/ids`, `/experimental/resource`, `/project`, `/project/{id}/directories`), and watching and stopping workflow runs (`GET /workflow`, `GET /workflow/run`, `GET /workflow/run/{id}`, `POST …/wait`, `POST …/abort`) |
| `admin` | The control plane, the operator | everything: config, provider credentials (`/auth`, `/provider`, `/config/providers`), MCP management and OAuth, terminals (`/pty`), the session `shell` command, saving and starting workflows, `/tui/*`, message and part edits and deletes, `/instance/dispose`, the `/global` routes, sync, sharing, `/experimental/*`, the v2 `/api` surface, the web UI |

### Threat model: what each scope can cause

The principle: **a `chat` token can converse with the agent and answer its
asks. It can never execute anything except through the model, under the
operator's permission policy, and it can never change that policy.**
`connector` is `chat` with a wider view; it obeys the same principle. Only
`admin` (and Basic auth, which is the operator) executes directly or sets
policy.

What a holder of a `chat` or `connector` token **can** cause:

- Whatever the agent does in the allowed project when prompted, to the
  extent the operator's `permission` config allows it or asks. If `bash` is
  `allow`, a prompt can get shell run: that is the operator's policy, not a
  bypass of it. Scope the policy for the least trusted token holder.
- Answering the agent's permission asks `once` or `reject`, and its
  questions. An `ask` rule is therefore a decision delegated to whoever
  holds a `chat` token; a `deny` rule is not theirs to lift.
- Choosing among the agents a client's picker shows (not hidden, not
  subagent-only) and among the configured models. An agent's own
  `permission` block is part of the operator's policy: if one visible agent
  is broader than another, a token holder can pick it. Disable agents that
  should not be reachable.
- Running the configured custom commands (`/name`). The template is the
  operator's; the caller's arguments are data.
- Undoing the agent's own edits (`revert` / `unrevert`), which restores
  project files from the snapshots its turns took; forking, summarising,
  renaming, archiving and deleting sessions; aborting a turn.
- Switching tools *off* for a session (`tools: { name: false }`).
- Reading and listing files inside the project, and its git status and
  diff.
- Spending the operator's model budget.

What it **cannot** cause, and what stops it:

| It cannot | Because |
| --- | --- |
| Run shell directly | `POST /session/{id}/shell`, `/pty`, `/tui/*` and workflow scripts are `admin` |
| Inject shell through a command's arguments | Inline shell (`` !`…` ``) is taken from the configured template only; arguments reach it as shell parameters (`$1`, `$ARGUMENTS`) and are never scanned for `` !`…` `` or placeholders. This holds for every caller, the operator included |
| Run a template's inline shell that the policy would ask about or deny | Below `admin`, a command whose template contains inline shell runs only if the `bash` permission of the agent and session allows that exact command outright; otherwise the command is refused before anything runs |
| Grant itself permissions | `permission` on session create and update is refused (403); `tools: { name: true }` (which becomes an allow rule) is refused |
| Make a standing allow rule | `always` on a permission reply is refused unless `server.auth.bearer.reply_always` is `true` |
| Use a hidden or subagent-only agent, or start a subtask directly | `agent` on prompts, commands and session create is checked; `subtask` parts are refused |
| Rewrite what the model reads as its history | Message deletes and part edits and deletes are `admin` |
| Answer another project's asks | A reply looks its ask up by id across loaded projects only within `server.directories` |
| Reach files outside the project | See the next section |
| Read or change config, credentials or MCP servers | Those routes are `admin` |

What remains with the operator to decide: the `permission` config is the
whole of what a token holder can make the agent do. A server whose config
allows `bash` to every agent gives every `chat` token holder shell in the
project, through the model. For an unattended agent that is usually the
point; when it is not, set `bash` (and `edit`, `webfetch`, …) to `ask` or
`deny` and decide who holds tokens that can answer.

### What a token narrower than `admin` is confined to

- **Directories.** The directory a request resolves to must be one of
  `server.directories` or inside one, however it is named: `?directory=`,
  the `x-opencode-directory` header, the directory of the session in the
  path, or the server's working directory when nothing is named. Symlinks
  and `..` are resolved first. Selecting a workspace is refused.
- **Files.** `GET /file` lists only inside the project; a `file:` part
  attached to a prompt and an `@path` mentioned in a command's arguments
  must be inside it too. (The operator's clients may name any file.)

Basic auth, `admin` tokens and a server with no authentication are not
confined.

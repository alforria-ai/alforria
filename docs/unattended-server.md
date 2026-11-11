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

Each scope includes the ones above it in this table. **A route that is not
listed needs `admin`**, so a route added later — a whole new family included
— is closed to narrower tokens until it is listed
(`crates/alforria-server/src/middleware/scope.rs`).

| Scope | For | Routes |
| --- | --- | --- |
| `chat` | A person talking to the agent from a browser or a phone | `GET /global/health`, `GET /event`; sessions: `GET`/`POST /session`, `GET /session/status`, `GET`/`PATCH`/`DELETE /session/{id}`, `…/children`, `…/todo`, `…/diff`, `…/goal`, `GET`/`POST …/message`, `GET …/message/{id}`, `…/prompt_async`, `…/command`, `…/abort`, `…/fork`, `…/init`, `…/summarize`, `…/revert`, `…/unrevert`, `…/permissions/{id}`; replies: `GET /permission`, `POST /permission/{id}/reply`, `GET /question`, `POST /question/{id}/reply`, `…/reject`; files in the project: `GET /file`, `/file/content`, `/file/status`, `/find`, `/find/file`, `/find/symbol`, `POST /file/ticket`, `GET /file/raw`, `PUT /file/content`; and what a client draws itself with: `GET /agent`, `/command`, `/skill`, `/path`, `/project/current`, `/vcs`, `/vcs/status`, `/vcs/diff` |
| `connector` | A trusted sidecar on the same machine (a messaging bridge, a scheduler) | everything in `chat`, plus `GET /global/event` (every directory's events on one stream), `POST /log`, status reads (`GET /mcp`, `/lsp`, `/formatter`, `/experimental/tool`, `/experimental/tool/ids`, `/experimental/resource`, `/project`, `/project/{id}/directories`), and watching and stopping workflow runs (`GET /workflow`, `GET /workflow/run`, `GET /workflow/run/{id}`, `POST …/wait`, `POST …/abort`) |
| `admin` | The control plane, the operator | everything: config, provider credentials (`/auth`, `/provider`, `/config/providers`), MCP management and OAuth, terminals (`/pty`), the session `shell` command, saving and starting workflows, `/tui/*`, message and part edits and deletes, `/instance/dispose`, the `/global` routes, sync, sharing, `/experimental/*`, the v2 `/api` surface, the web UI |

### Threat model: what each scope can cause

The principle: **a `chat` token can converse with the agent and answer its
asks. It can never execute anything except through the model, under the
operator's permission policy, and it can never change that policy.**
`connector` is `chat` with a wider view and obeys the same principle. Only
`admin` (and Basic auth, which is the operator) executes directly or sets
policy.

How it is enforced: the scope table decides which routes a token reaches.
What a prompt or command from it may *contain* is not checked in the routes
at all. A route marks the request as coming from a caller below `admin`
(`Caller::Restricted`, `crates/alforria-core/src/session/caller.rs`), and the
engine judges the values it actually resolves, at the point it uses them:
the agent the turn will run as, the parts as they will be stored, the file
about to be read. There is no second spelling of a value for a check to look
at.

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
| Run as a hidden or subagent-only agent | The agent a turn resolves to — named in the request, configured on the command, or the default — is checked before the turn is stored. An omitted agent means the default agent, never the session's stored one |
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

#### Every field a `chat` token can send

| Request | Field | What the engine derives from it | Where it is judged |
| --- | --- | --- | --- |
| any | `?directory=`, `x-opencode-directory` | the project instance | auth layer and location layer: must equal a `server.directories` entry |
| any | `?workspace=` | a workspace | refused (auth layer) |
| any `/session/{id}/…` | the session id | the project (the session's own directory) | location layer, same allowlist |
| `POST /session` | `title`, `metadata` | stored text | not restricted |
| | `parentID` | a child session | allowed; the child cannot then be prompted by this token |
| | `agent`, `model` | stored on the session | not at create: the agent is judged when a turn runs, and an omitted agent on a prompt never falls back to it |
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
| `POST …/fork`, `…/revert` | `messageID`, `partID` | a point in the session's own history | must belong to the session; not otherwise restricted |
| `POST /permission/{id}/reply`, `…/permissions/{id}` | `reply` / `response` | the answer to an ask | route: `always` refused; the ask must be in an allowed project |
| | `message` | feedback text for the model | not restricted |
| `POST /question/{id}/reply`, `/reject` | `answers` | the answer | the question must be in an allowed project |
| `GET /file`, `/file/content`, `/file/raw`, `POST /file/ticket` | `path` | a file or directory read | route: inside the project, resolved |
| `PUT /file/content` | `path` | a file write | route: inside the upload area, not a loader name |
| `GET /find*`, `/vcs*` | `query`, `pattern`, `mode`, … | a search or diff of the project | no caller-chosen path |

The checks left in routes (`permission` and `workspaceID` on session create
and update, `always` on a reply, file paths) are on requests that never
become a prompt; each judges the very value the handler then uses.

#### What stays with the operator

- The `permission` config is the whole of what a token holder can make the
  agent do. A server whose config allows `bash` to every agent gives every
  `chat` token holder shell in the project, through the model. For an
  unattended agent that is usually the point; when it is not, set `bash`
  (and `edit`, `webfetch`, …) to `ask` or `deny` and decide who holds tokens
  that can answer.
- **`edit` / `write` permission on the project includes the project's own
  config and instructions.** The model can write `opencode.json`,
  `.opencode/` (agents, commands with inline shell), `AGENTS.md`,
  `.claude/` skills and workflows or `.git/hooks` with its ordinary tools
  whenever the policy lets it edit files there, and the next instance of that
  project loads them. The upload route refuses these names to tokens below
  `admin`; the agent's tools are governed by the `permission` config, which
  is the operator's to write. To keep project files from carrying policy at
  all:
  - put the policy in the managed config directory (`/etc/opencode`), which
    is merged last and wins over anything in the project, and which the
    agent's user should not be able to write;
  - or set `OPENCODE_DISABLE_PROJECT_CONFIG=1`, so project `opencode.json`
    files and `.opencode/` directories are not loaded at all;
  - and deny `edit` on those paths in the `permission` config.
- The host config (`server.*`, `shell_env`, `hook.*`) is never read from the
  project, whatever the agent writes there.

### What a token narrower than `admin` is confined to

- **Directories.** The directory a request resolves to must be exactly one
  of `server.directories`, however it is named: `?directory=`, the
  `x-opencode-directory` header, the directory of the session in the path,
  or the server's working directory when nothing is named. Symlinks and
  `..` are resolved first. A directory *below* a listed one is not allowed:
  opened as a project of its own it would load config from inside the
  project. Selecting a workspace is refused.
- **Files.** Reads and listings stay inside the project; uploads go to the
  upload area (see "Files for a browser").

Basic auth, `admin` tokens and a server with no authentication are not
confined.

## Files for a browser: tickets, raw reads, uploads

The frozen file routes return JSON and need an `Authorization` header. A
browser showing a workspace image in an `<img>`, or offering a download
link, can send neither a header nor parse JSON; and nothing could put a file
*into* the workspace. Three routes outside the frozen tables cover that,
plus one addition to a frozen route. All take `?directory=` (or the
`x-opencode-directory` header) like the rest of the v1 surface, and need the
`chat` scope.

Every `path` is relative to the project directory (an absolute path is
accepted when it is inside). `..` and existing symlinks are resolved first,
and the result must be inside the project directory or its git worktree —
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

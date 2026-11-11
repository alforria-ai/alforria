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

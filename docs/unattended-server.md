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

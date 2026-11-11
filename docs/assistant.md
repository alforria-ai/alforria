# Always-on assistant features

alforria can run as an assistant that works without anyone at the keyboard:
turns that start on a schedule or an event, memory that outlives a session,
history imported from another tool, and switches that let a product replace
the coding-agent persona. All of it is an alforria extension: none of it
exists in upstream opencode, the routes sit outside the frozen v1/v2 tables,
and the tables are created by alforria's own migrations
(`crates/alforria-core/src/storage/migrations/`).

This page is the reference for those features. The public docs at
alforria.ai are written from it.

## Triggers

A trigger pairs something to wait for with a prompt. When it fires, the
prompt starts a turn through the same path as `prompt_async`, in an ordinary
session that any client can open.

### Declaring triggers

Triggers are read from your own config only: the global config
(`~/.config/alforria/opencode.json`), the `.opencode` directory in your home,
the files named by `OPENCODE_CONFIG`, `OPENCODE_CONFIG_DIR` and
`OPENCODE_CONFIG_CONTENT`, and the managed config. A `trigger` block in a
project's `opencode.json` or `.opencode/`, or in a config fetched from a
remote, is ignored with a warning that names the file. A trigger starts turns
with nobody watching, under rules the config chooses, so a cloned repository
must not be able to declare one. There is no per-project trust setting. Use
`POST /trigger` (which takes `?directory=`) for triggers that belong to one
project.

```jsonc
{
  "trigger": {
    "morning-brief": {
      "on": { "cron": "0 9 * * 1-5", "timezone": "Europe/Paris" },
      "prompt": "Write today's brief from memory/ and the calendar.",
      "session": "new",
      "report": "final"
    },
    "heartbeat": {
      "on": { "interval": "30m" },
      "prompt": "Check HEARTBEAT.md. If nothing needs doing, answer HEARTBEAT_OK.",
      "session": "reuse",
      "quiet_token": "HEARTBEAT_OK"
    },
    "inbox": {
      "on": { "watch": { "path": "inbox/", "debounce": "60s" } },
      "prompt": "File what landed in inbox/.",
      "permission": { "bash": "deny" }
    },
    "deploys": { "on": { "webhook": true }, "prompt": "A deploy finished. Check it." }
  },
  "triggers": { "enabled": true, "concurrency": 1, "max_watched": 10000 }
}
```

| Field | Meaning |
| --- | --- |
| `on` | Exactly one of `cron` (+ optional `timezone`), `interval`, `at`, `watch`, `webhook: true`. |
| `prompt` | What the turn is asked. `@file` mentions resolve as they do in a command. |
| `title` | Names the session (default: the trigger id). |
| `agent`, `model` | Agent name and `provider/model`; the defaults otherwise. |
| `session` | `"new"` (default) makes a session per run, titled `<title> · <time>`. `"reuse"` appends to one long-lived session per trigger. A session id runs in that session. |
| `permission` | A permission block like an agent's. It becomes the session's own ruleset. |
| `report` | `"final"` (default) delivers the final answer; `"none"` never does. |
| `quiet_token` | A final answer that is exactly this text (ignoring surrounding whitespace) means "nothing to report". |
| `enabled` | `false` keeps the trigger without running it. |
| `catch_up` | Fire once at startup if a tick was missed while the server was down. Default `false`; a one-shot `at` defaults to `true`. |

- `cron` takes five fields, names (`MON-FRI`, `JAN`), and `@hourly`, `@daily`,
  `@weekly`, `@monthly`, `@yearly`. `timezone` is an IANA name (default UTC).
  Daylight saving is handled: a daily job keeps its local hour, a time inside
  the spring gap runs once right after it, and a time that happens twice in
  autumn runs once.
- `interval` and `debounce` are durations: `90s`, `30m`, `2h`, `1d`, `1h30m`.
  The shortest interval is 10 seconds.
- `watch` paths are relative to the project directory. A stored trigger must
  stay inside it; your config may name any path. Symlinks are resolved before
  that check (through the nearest existing parent when the path does not
  exist yet) and again when the watcher is installed, and links inside a
  watched tree are not followed. `.git` and alforria's own data, state and
  cache directories are never reported, and a tree with more than
  `triggers.max_watched` entries is refused.
- A run started by a `webhook` or a `watch` is read-only unless the
  trigger's configuration grants more. Its prompt carries text from outside:
  the body someone posted, or the names of files someone dropped. The rules
  of such a run (and of any run that carries a payload) are the intersection
  of what the trigger would otherwise be allowed and a read-only ceiling. For
  every permission and pattern the stricter of the two applies, and an `ask`
  that nobody can answer is a deny:
  - `read`, `grep`, `glob`, `list`, `lsp` and `todowrite` keep exactly the
    answer the agent's rules (and, for a trigger an agent scheduled, its
    creator's rules) already gave. A path the agent may not read stays
    unreadable, the default ask on `.env` files becomes a deny, and
    `external_directory` still stops all of them from leaving the project.
  - Everything else is denied and removed from the request: the shell,
    edits, the network, subagents, skills, scheduling, memory, the `question`
    tool, every MCP tool, and any tool added later.
  - An explicit grant in the trigger's own `permission` block is the only way
    to widen such a run, and it grants exactly what it names, for example
    `"permission": { "bash": { "make deploy": "allow" } }`. For a trigger an
    agent scheduled, a grant is still bounded by its creator's permissions:
    whatever the creator's rules withheld stays withheld.
  - A `webhook` or `watch` trigger always runs in a session of its own
    (`"new"` or `"reuse"`), never in an existing conversation.
- Unattended turns have nobody to answer a permission prompt. Give a trigger
  explicit `allow`/`deny` rules for what it needs.

Triggers created while running (the `schedule` tool, `POST /trigger`) are
stored in the `trigger` table and survive restarts. A stored trigger cannot
take the id of a config trigger. Config triggers can be paused and resumed at
run time, but only the config changes or removes them.

### How runs behave

- A trigger has at most one run at a time. A tick that arrives while the last
  run is still going, or still waiting for a slot, is recorded as `skipped`
  and announced; it is not queued.
- Across triggers, `triggers.concurrency` runs go at once (default 1). The
  others wait their turn, at most one per trigger.
- Ticks missed while the server was down are not replayed. With
  `catch_up: true` the most recent missed tick runs once at startup. A machine
  that slept through several ticks also fires once.
- Two servers sharing a database run each scheduled tick once: the tick is
  claimed in the database before it fires.
- A run cut short by a restart is closed as `failed`.
- The scheduler runs in `alforria serve`, `alforria web` and the TUI. `serve`
  and `web` boot every directory that has triggers at startup. One-shot
  commands (`alforria run`, `acp`) never fire triggers.
- The turn's user message is marked: each text part of the prompt carries
  `metadata.trigger = { "id": "<trigger id>", "run": "<run id>" }`, and the
  session's metadata carries `triggerID`.
- A webhook body (up to 256 KB) and the list of changed paths of a `watch`
  reach the model as a separate synthetic part, fenced in `<untrusted_data>`
  tags with every `<` escaped, and described as data, not instructions.

### Routes

All take `?directory=` like the v1 API and return plain JSON.

| Route | |
| --- | --- |
| `GET /trigger` | Every trigger, from the config and stored. |
| `POST /trigger` | Create a stored trigger. Body: a trigger, plus an optional `id`. |
| `GET /trigger/{id}` | One trigger. |
| `PATCH /trigger/{id}` | JSON merge patch (`null` removes a key; a new `on` replaces the old one). A config trigger accepts only `{ "enabled": bool }`. |
| `DELETE /trigger/{id}` | `{ "deleted": bool }`. |
| `POST /trigger/{id}/run` | Fire now. Returns the run. |
| `POST /trigger/{id}/hook` | The webhook entry, for `webhook: true` triggers. The raw body is the payload. Returns the run. |
| `GET /trigger/{id}/runs` | Runs, newest first (`?limit=`, up to 200 are kept). |

A trigger is returned as its own fields plus `id`, `source`
(`"config"` or `"runtime"`), `enabled`, `running`, `sessionID` (for `reuse`),
`originSessionID` (when an agent created it), `error` (why it cannot run) and
`time: { created, updated, lastFired?, next? }`.

A run is `{ id, triggerID, status, reason, sessionID?, scheduled?,
time: { started, ended? }, quiet?, report?, text?, error?, cause? }`.
`status` is `queued`, `running`, `completed`, `failed` or `skipped`;
`reason` is `schedule`, `catch_up`, `watch`, `webhook` or `manual`.

The webhook route is authenticated like every other route. It has no secret
of its own, so put the server's credentials on the caller.

Errors: 400 for an invalid trigger, 403 when triggers are disabled, 404 for an
unknown id, 409 for a conflict (a taken id, editing a config trigger, a hook on
a trigger that is not a webhook trigger).

### Events

| Event | Data |
| --- | --- |
| `trigger.fired` | `{ triggerID, runID, reason, scheduled? }` |
| `trigger.completed` | `{ triggerID, runID, sessionID, duration, quiet, report, text? }`. `report` is `"final"` (deliver `text`) or `"none"` (a quiet run, or `report: "none"`; no `text`). |
| `trigger.failed` | `{ triggerID, runID, sessionID?, duration, error }` |
| `trigger.skipped` | `{ triggerID, runID, reason, cause, scheduled? }` |
| `trigger.updated` | `{ triggerID }`, when a trigger is created, changed or deleted. |

`duration` is in milliseconds and includes time spent waiting for a slot.

### The `schedule` tool

The agent manages its own triggers with the built-in `schedule` tool:
`create`, `list`, `update`, `pause`, `resume` and `delete`. `create` takes a
`prompt` and one of `cron` (+ `timezone`), `interval`, `at` (RFC 3339), `in`
(a delay such as `"20m"`), `watch` (+ `debounce`) or `webhook: true`, and
optionally `id`, `title`, `session` (`new`, `reuse`, or `current` for this
conversation), `report` and `quiet_token`.

A trigger created by the tool runs as the agent that created it. Its sessions
carry a copy of the rules that governed the creating session at that moment
(the agent's rules, then the session's own), applied again on every run after
the agent's current rules, so a run can never do more than its creator could
when it asked. The tool cannot set `agent`, `model` or `permission`.

The tool only changes triggers from its own conversation tree. The session
that created a trigger, and that session's ancestors, may update, pause,
resume and delete it; an update leaves the copied rules as they are. A
descendant (a subagent, whose rules may be narrower) may only pause or delete
it. Triggers from the config, triggers created over HTTP and triggers of other
conversations are listed but cannot be changed, paused or deleted by the
tool: they are the user's. A run started by a trigger, and a subagent, are
bound the same way when they schedule something themselves, and agents may
hold at most 50 triggers per directory between them. One-shot triggers that
have fired are removed a day later.

Every action except `list` asks for the `schedule` permission, with the
action as the pattern:

```jsonc
{ "permission": { "schedule": { "create": "ask", "delete": "ask", "*": "allow" } } }
```

`"schedule": "deny"` removes the tool from the request. `triggers.enabled:
false` (or `OPENCODE_DISABLE_TRIGGERS=1`) turns the whole feature off.

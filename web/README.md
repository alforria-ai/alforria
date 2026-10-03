# alforria web

## Dev harness

Both scripts drive the real `target/debug/alforria serve` against a scripted
OpenAI-compatible backend. All their state (HOME, XDG dirs, projects) lives
under `web/.dev/` (gitignored), so they never touch your own alforria data. They
warn when the binary is older than the Rust sources: run
`cargo build -p alforria`.

```sh
bun run server            # isolated server on :4697 with a seeded demo fleet
bun run dev               # vite on :4610, proxying the API to :4697
```

`bun run server` wipes `web/.dev/` and seeds three git projects
(`web/.dev/projects/{atlas-api,lumen-web,ferrite-cli}`) through the API. The
sessions it leaves cover the states the UI has to show: finished (with todos
and edits), waiting on a bash permission, waiting on a `.env` read
permission, waiting on a question, a subagent with its child session, a
provider error, and one session that streams for ~60 s. Replies and follow-up
prompts work: answered asks resume their scripts, and any other prompt gets a
short fallback reply. Flags:

- `--lan`: bind 0.0.0.0 so other devices can reach the server directly.
- `--keep`: reuse the previous `web/.dev/` state. Pending asks and busy turns
  do not survive a restart.
- `--busy-seconds N`: how long the busy session streams.
- `ALFORRIA_PORT=N`: server port (the backend takes N+1).

The fleet's scripts are in `script/lib/fleet.ts`.

```sh
bun run record                    # every scenario
bun run record permission-gate    # one scenario
```

`bun run record` replays the e2e fixtures
(`crates/alforria/tests/e2e_agent/fixtures/a{1,2,3,4}_*.json`) against a fresh
server per scenario and writes what a client sees to `test/fixtures/`:

- `<scenario>.events.jsonl`: every `/global/event` frame in order, one JSON
  object per line, starting with `server.connected`.
- `<scenario>.bootstrap.json`: `project`, `session`, `sessionStatus`,
  `permission` and `question` as fetched before the prompt, plus `meta`.
  `meta.eventIndex` is the number of frames already reflected in the
  snapshot; `meta.baseTime` is the wall clock when recording started.
- `<scenario>.final.json`: the same lists after the root session settles,
  plus `messages[sessionID]` from `GET /session/{id}/message` for every
  session, subagent children included.

Ids and timestamps are the server's own. The only rewrite is the local
recording root, which becomes `/fixture/<scenario>`; pass `--raw` to keep the
real paths. Git commits use fixed dates, so project ids are the same on every
run. Recording refuses a stale binary unless you pass `--allow-stale`.

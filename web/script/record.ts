// `bun run record [scenario…] [--raw] [--allow-stale]`
//
// Records exactly what a web client sees while an e2e scenario runs against
// an isolated `alforria serve` with the scripted backend. Per scenario it
// writes, under web/test/fixtures/:
//   <scenario>.events.jsonl   every /global/event frame, in order, one per line
//                             (starting with `server.connected`)
//   <scenario>.bootstrap.json the REST snapshots a client takes before the
//                             prompt, plus `meta` (ids, base time, the index of
//                             the first frame after the snapshot)
//   <scenario>.final.json     the same lists after the session settles, and
//                             `GET /session/{id}/message` for every session
//
// Ids and timestamps are the server's own. The only rewrite is the local
// recording root (web/.dev/record/<scenario>), replaced by
// /fixture/<scenario> so fixtures do not depend on the checkout path;
// `--raw` keeps real paths.

import { realpathSync, rmSync, mkdirSync, writeFileSync, existsSync, statSync } from "node:fs"
import { join } from "node:path"
import {
  Api,
  BIN,
  DEV,
  EventStream,
  MODEL_REF,
  REPO,
  WEB,
  ensureBinary,
  gitProject,
  isolatedEnv,
  providerConfig,
  spawnServe,
  writeJson,
  type GlobalFrame,
} from "./lib/alforria"
import { loadFixture, queue, startBackend } from "./lib/backend"

type Scenario = {
  name: string
  // e2e fixture under crates/alforria/tests/e2e_agent/fixtures/
  fixture: string
  // The e2e driver's prompt, so the request history matches the fixture.
  prompt: string
  // What the title fork answers.
  title: string
  files?: Record<string, string>
  // Answer every `permission.asked` with this reply.
  reply?: "once" | "always" | "reject"
  // Event types the recording must contain.
  expect: string[]
}

const SCENARIOS: Scenario[] = [
  {
    name: "file-mutation",
    fixture: "a1_file_mutation",
    prompt: "create notes.md for me",
    title: "Create notes.md",
    expect: ["message.part.updated", "session.status"],
  },
  {
    name: "multi-step",
    fixture: "a2_multi_step",
    prompt: "read the two files and answer",
    title: "Read a.txt and b.txt",
    files: { "a.txt": "alpha\n", "b.txt": "beta\n" },
    expect: ["message.part.updated", "session.status"],
  },
  {
    name: "permission-gate",
    fixture: "a3_permission_gate",
    prompt: "read twice",
    title: "Read secret.env twice",
    files: { "secret.env": "TOKEN=1\n" },
    reply: "once",
    expect: ["permission.asked", "permission.replied", "session.status"],
  },
  {
    name: "subagent",
    fixture: "a4_subagent",
    prompt: "spawn a subagent",
    title: "Delegate research",
    expect: ["session.created", "message.part.updated", "session.status"],
  },
]

const args = process.argv.slice(2)
const raw = args.includes("--raw")
const names = args.filter((arg) => !arg.startsWith("--"))
const selected = names.length ? SCENARIOS.filter((scenario) => names.includes(scenario.name)) : SCENARIOS
const unknown = names.filter((name) => !SCENARIOS.some((scenario) => scenario.name === name))
if (unknown.length) {
  console.error(`unknown scenario(s): ${unknown.join(", ")}; known: ${SCENARIOS.map((s) => s.name).join(", ")}`)
  process.exit(2)
}

const { stale } = await ensureBinary()
// Fixtures get committed: refuse to record an outdated engine by accident.
if (stale && !args.includes("--allow-stale")) {
  console.error("refusing to record with a stale binary (pass --allow-stale to override)")
  process.exit(1)
}

const OUT = join(WEB, "test/fixtures")
mkdirSync(OUT, { recursive: true })
const commit = Bun.spawnSync(["git", "rev-parse", "HEAD"], { cwd: REPO }).stdout.toString().trim()

for (const scenario of selected) {
  const started = performance.now()
  const result = await record(scenario)
  const seconds = ((performance.now() - started) / 1000).toFixed(1)
  console.log(`${scenario.name}: ${result.frames} frames, ${result.messages} messages (${seconds}s)`)
  for (const file of result.files) console.log(`  ${file}`)
}

async function record(scenario: Scenario) {
  const root = join(DEV, "record", scenario.name)
  rmSync(root, { recursive: true, force: true })
  const project = join(root, "project")
  const backend = startBackend({ pick: queue(loadFixture(scenario.fixture)), title: () => scenario.title })
  gitProject(project, { "README.md": `# ${scenario.name}\n`, ...scenario.files })
  // Written after the commit: the backend port varies per run, and the root
  // commit (hence the project id) must not.
  writeJson(join(project, "opencode.json"), providerConfig(backend.url, { title: `record ${scenario.name}` }))

  const serve = await spawnServe({
    cwd: project,
    env: isolatedEnv(root),
    hostname: "127.0.0.1",
    port: 0,
    log: join(root, "server.log"),
  })
  const api = new Api(serve.url, project)
  try {
    const baseTime = Date.now()
    const stream = await EventStream.open(serve.url)
    const created = stream.next((frame) => frame.payload.type === "session.created", 10_000)
    const session = await api.post("/session", {})
    // `session.created` (and its sync mirror) lands after the POST returns;
    // let it arrive so frames before `eventIndex` are all reflected in the
    // snapshot below, which is taken before the prompt.
    await created
    await Bun.sleep(250)
    const eventIndex = stream.frames.length
    const bootstrap = {
      meta: {
        scenario: scenario.name,
        fixture: `crates/alforria/tests/e2e_agent/fixtures/${scenario.fixture}.json`,
        prompt: scenario.prompt,
        sessionID: session.id,
        directory: project,
        baseTime,
        eventIndex,
        recordedAt: new Date(baseTime).toISOString(),
        alforria: { commit, built: new Date(statSync(BIN).mtimeMs).toISOString() },
      },
      ...(await snapshot(api, serve.url)),
    }

    const settled = settle(stream, api, session.id, scenario.reply)
    await api.post(`/session/${session.id}/prompt_async`, {
      parts: [{ type: "text", text: scenario.prompt }],
      model: MODEL_REF,
    })
    await settled

    const lists = await snapshot(api, serve.url)
    const messages: Record<string, unknown> = {}
    for (const item of lists.session as { id: string }[]) {
      messages[item.id] = await api.get(`/session/${item.id}/message`)
    }
    stream.close()

    const rewrite = (text: string) => (raw ? text : text.replaceAll(realpathSync(root), `/fixture/${scenario.name}`))
    const base = join(OUT, scenario.name)
    const files = [`${base}.events.jsonl`, `${base}.bootstrap.json`, `${base}.final.json`]
    writeFileSync(files[0]!, rewrite(stream.frames.map((frame) => JSON.stringify(frame) + "\n").join("")))
    writeFileSync(files[1]!, rewrite(JSON.stringify(bootstrap, null, 2) + "\n"))
    writeFileSync(files[2]!, rewrite(JSON.stringify({ ...lists, messages }, null, 2) + "\n"))
    for (const file of files) {
      if ((await Bun.file(file).text()).includes(REPO)) console.warn(`  warning: ${file} still contains ${REPO}`)
    }
    const types = new Set(stream.frames.map((frame) => frame.payload.type))
    const missing = scenario.expect.filter((type) => !types.has(type))
    if (missing.length) throw new Error(`${scenario.name}: recording lacks ${missing.join(", ")}`)
    return {
      frames: stream.frames.length,
      messages: Object.values(messages).flat().length,
      files: files.map((file) => file.slice(WEB.length + 1)),
    }
  } finally {
    await serve.stop()
    backend.stop()
  }
}

// The REST state a client bootstraps from (bootstrap.json keys).
async function snapshot(api: Api, base: string) {
  return {
    project: await new Api(base).get("/project"),
    session: await api.get("/session"),
    sessionStatus: await api.get("/session/status"),
    permission: await api.get("/permission"),
    question: await api.get("/question"),
  }
}

// Resolves once the root session went busy → idle and the stream has been
// quiet (heartbeats aside) for a second, answering permission asks with
// `reply` meanwhile.
function settle(stream: EventStream, api: Api, sessionID: string, reply?: string) {
  return new Promise<void>((ok, fail) => {
    let busy = false
    let idle = false
    let last = Date.now()
    const deadline = setTimeout(() => done(new Error(`session ${sessionID} did not settle within 90s`)), 90_000)
    const poll = setInterval(() => {
      if (idle && Date.now() - last > 1_000) done()
    }, 100)
    const off = stream.on((frame: GlobalFrame) => {
      const { type, properties } = frame.payload
      if (type === "server.heartbeat") return
      last = Date.now()
      if (type === "permission.asked" && reply) {
        api.post(`/permission/${properties.id}/reply`, { reply }).catch(done)
      }
      if (type === "session.status" && properties.sessionID === sessionID) {
        if (properties.status.type !== "idle") busy = true
        idle = busy && properties.status.type === "idle"
      }
    })
    function done(error?: Error) {
      clearTimeout(deadline)
      clearInterval(poll)
      off()
      error ? fail(error) : ok()
    }
  })
}

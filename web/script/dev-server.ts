// `bun run server [--lan] [--keep] [--busy-seconds N]`
//
// An isolated `alforria serve` for web client development, on a fixed port
// (4697, or ALFORRIA_PORT), talking to a scripted OpenAI-compatible backend
// on the next port. It seeds a demo fleet through the real API (see
// lib/fleet.ts) and runs until Ctrl-C.
//
// All state lives under web/.dev/ (projects/ and server/), wiped on every
// start unless --keep. --lan binds 0.0.0.0 so a phone on the LAN can reach
// the server directly; `bun run dev` proxies to 127.0.0.1 either way.

import { existsSync, readFileSync, rmSync } from "node:fs"
import { networkInterfaces } from "node:os"
import { join } from "node:path"
import {
  Api,
  DEV,
  EventStream,
  MODEL_REF,
  ensureBinary,
  gitProject,
  isolatedEnv,
  providerConfig,
  spawnServe,
  waitUntil,
  writeJson,
} from "./lib/alforria"
import { keyed, startBackend } from "./lib/backend"
import { startLibertai } from "./lib/libertai"
import { fleet, scripts, type State } from "./lib/fleet"

const args = process.argv.slice(2)
const lan = args.includes("--lan")
const keep = args.includes("--keep")
const busyFlag = args.indexOf("--busy-seconds")
const busySeconds = busyFlag >= 0 ? Number(args[busyFlag + 1]) : 60
const PORT = Number(process.env.ALFORRIA_PORT ?? 4697)

const PROJECTS = join(DEV, "projects")
const STATE = join(DEV, "server")
const FLEET = join(STATE, "fleet.json")

await ensureBinary()
if (!keep) {
  rmSync(PROJECTS, { recursive: true, force: true })
  rmSync(STATE, { recursive: true, force: true })
}

const projects = fleet(busySeconds)
const routes = keyed(scripts(projects))
// A fixed port keeps the committed opencode.json files unchanged under --keep.
const backend = startBackend({ ...routes, port: PORT + 1 })

// The provider is global too, so sessions a human starts in any other
// directory still reach the scripted backend.
// "Sign in with LibertAI" goes to a local stand-in, never the real console.
const libertai = startLibertai(PORT + 2)
const env = { ...isolatedEnv(STATE), ...libertai.env() }
writeJson(join(env.XDG_CONFIG_HOME!, "opencode/opencode.json"), providerConfig(backend.url))
for (const project of projects) {
  const dir = join(PROJECTS, project.name)
  const config = providerConfig(backend.url, { title: project.name, ...project.config })
  if (!existsSync(dir)) gitProject(dir, { ...project.files, "opencode.json": JSON.stringify(config, null, 2) + "\n" })
  // --keep with another ALFORRIA_PORT moves the backend.
  else writeJson(join(dir, "opencode.json"), config)
}

const serve = await spawnServe({
  cwd: join(PROJECTS, projects[0]!.name),
  env,
  hostname: lan ? "0.0.0.0" : "127.0.0.1",
  port: PORT,
  log: join(STATE, "server.log"),
}).catch((error) => {
  backend.stop()
  libertai.stop()
  console.error(`could not start alforria serve on port ${PORT}: ${error.message}`)
  process.exit(1)
})

let stopping = false
async function shutdown(code = 0) {
  if (stopping) return
  stopping = true
  console.log("\nstopping…")
  await serve.stop()
  backend.stop()
  libertai.stop()
  process.exit(code)
}
process.on("SIGINT", () => shutdown())
process.on("SIGTERM", () => shutdown())

type Seeded = { project: string; directory: string; id: string; title: string; state: State }

const api = new Api(serve.url)
const reused = existsSync(FLEET)
try {
  const seeded: Seeded[] = reused ? JSON.parse(readFileSync(FLEET, "utf8")) : await seed()
  await report(seeded)
} catch (error) {
  console.error(`seeding failed: ${error instanceof Error ? error.message : error}`)
  await shutdown(1)
}
console.log(
  reused
    ? "\n--keep: reused the seeded fleet; pending asks and busy turns do not survive a restart. Ctrl-C to stop."
    : `\nThe busy session streams for ~${busySeconds}s. Ctrl-C to stop.`,
)

// Create and prompt every session, then wait until each reaches its state.
async function seed(): Promise<Seeded[]> {
  const stream = await EventStream.open(serve.url)
  const status = new Map<string, string>()
  const ran = new Set<string>()
  stream.on((frame) => {
    const { type, properties } = frame.payload
    if (type !== "session.status") return
    status.set(properties.sessionID, properties.status.type)
    if (properties.status.type === "busy") ran.add(properties.sessionID)
  })

  const out: Seeded[] = []
  for (const project of projects) {
    const directory = join(PROJECTS, project.name)
    const at = api.at(directory)
    for (const def of project.sessions) {
      const session = await at.post("/session", {})
      await at.post(`/session/${session.id}/prompt_async`, {
        parts: [{ type: "text", text: def.script.prompt }],
        model: MODEL_REF,
      })
      out.push({ project: project.name, directory, id: session.id, title: def.script.title, state: def.state })
    }
  }

  const finished = (id: string) => ran.has(id) && status.get(id) === "idle"
  await Promise.all(
    out.map((item) => {
      const at = api.at(item.directory)
      const reached = async () => {
        switch (item.state) {
          case "idle":
            return finished(item.id)
          case "busy":
            return status.get(item.id) === "busy"
          case "permission":
            return (await at.get<{ sessionID: string }[]>("/permission")).some((ask) => ask.sessionID === item.id)
          case "question":
            return (await at.get<{ sessionID: string }[]>("/question")).some((ask) => ask.sessionID === item.id)
          case "error": {
            if (!finished(item.id)) return false
            const messages = await at.get<{ info: { error?: unknown } }[]>(`/session/${item.id}/message`)
            return messages.some((message) => message.info.error)
          }
        }
      }
      return waitUntil(`"${item.title}" to reach ${item.state}`, reached, 60_000)
    }),
  )
  stream.close()
  writeJson(FLEET, out)
  return out
}

// Print where to connect and what the fleet looks like right now.
async function report(items: Seeded[]) {
  const urls = [`http://127.0.0.1:${PORT}`]
  if (lan) {
    for (const nets of Object.values(networkInterfaces())) {
      for (const net of nets ?? [])
        if (net.family === "IPv4" && !net.internal) urls.push(`http://${net.address}:${PORT}`)
    }
  }
  console.log(`\nalforria dev server: ${urls.join("  ")}`)
  console.log(`scripted backend:    ${backend.url}`)
  console.log(`libertai stand-in:   ${libertai.url}`)
  console.log(`state:               ${DEV} (log: ${join(STATE, "server.log")})\n`)

  for (const project of projects) {
    const directory = join(PROJECTS, project.name)
    const at = api.at(directory)
    const [sessions, status, permissions, questions] = await Promise.all([
      at.get<{ id: string; title: string; parentID?: string }[]>("/session"),
      at.get<Record<string, { type: string }>>("/session/status"),
      at.get<{ sessionID: string; permission: string }[]>("/permission"),
      at.get<{ sessionID: string }[]>("/question"),
    ])
    console.log(`${project.name}  ${directory}`)
    for (const session of sessions) {
      const ask = permissions.find((item) => item.sessionID === session.id)
      const state = ask
        ? `waiting: permission (${ask.permission})`
        : questions.some((item) => item.sessionID === session.id)
          ? "waiting: question"
          : (status[session.id]?.type ?? "idle")
      const planned = items.find((item) => item.id === session.id)?.state
      const kind = session.parentID ? "  └ subagent" : ""
      console.log(
        `  ${session.id}  ${state.padEnd(30)} ${session.title}${kind}${planned === "error" ? "  (errored)" : ""}`,
      )
    }
  }
}

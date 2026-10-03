// Isolated `alforria serve` plumbing shared by dev-server.ts and record.ts:
// binary freshness, a scrubbed environment whose HOME/XDG dirs live under
// web/.dev/, deterministic git projects, the serve process, a v1 API client
// and a /global/event SSE reader.
//
// The user's real alforria data must never be touched: every path the
// server can write to is under the given root.

import { mkdirSync, statSync, writeFileSync, existsSync, openSync, closeSync, writeSync } from "node:fs"
import { dirname, join, resolve } from "node:path"

export const WEB = resolve(import.meta.dir, "../..")
export const REPO = resolve(WEB, "..")
// ALFORRIA_DEV_DIR lets a second harness (the e2e suite) run beside `bun run server`.
export const DEV = process.env.ALFORRIA_DEV_DIR ?? join(WEB, ".dev")
export const BIN = join(REPO, "target/debug/alforria")

export const PROVIDER = "mock"
export const MODEL = "mock-model"
export const MODEL_REF = { providerID: PROVIDER, modelID: MODEL }

// Build the binary only when it is missing; a stale one is the user's call
// (a rebuild takes minutes), so just say so.
export async function ensureBinary(): Promise<{ stale?: string }> {
  if (!existsSync(BIN)) {
    console.log("target/debug/alforria is missing; running `cargo build -p alforria` once…")
    const build = Bun.spawnSync(["cargo", "build", "-p", "alforria"], {
      cwd: REPO,
      stdout: "inherit",
      stderr: "inherit",
    })
    if (build.exitCode !== 0) throw new Error("cargo build -p alforria failed")
    return {}
  }
  const built = statSync(BIN).mtimeMs
  for await (const file of new Bun.Glob("crates/**/*.rs").scan({ cwd: REPO })) {
    if (statSync(join(REPO, file)).mtimeMs > built) {
      console.warn(
        `\n  warning: target/debug/alforria is older than ${file}.\n` +
          "  Run `cargo build -p alforria` to test the current source.\n",
      )
      return { stale: file }
    }
  }
  return {}
}

// The env for every child that may touch alforria or git state: caller
// credentials and OPENCODE_* overrides are stripped, HOME and XDG dirs
// point under `root`, and the models.dev catalog is an empty local file.
export function isolatedEnv(root: string): Record<string, string> {
  const env: Record<string, string> = {}
  for (const [key, value] of Object.entries(process.env)) {
    if (value === undefined) continue
    if (/^(OPENCODE_|LIBERTAI_|ANTHROPIC_|OPENAI_)/.test(key)) continue
    env[key] = value
  }
  const home = join(root, "home")
  const models = join(root, "models.json")
  mkdirSync(home, { recursive: true })
  writeFileSync(models, "{}")
  Object.assign(env, {
    HOME: home,
    OPENCODE_TEST_HOME: home,
    OPENCODE_MODELS_PATH: models,
    OPENCODE_DISABLE_MODELS_FETCH: "1",
    LIBERTAI_MODEL_CATALOG_URL: "",
    XDG_CONFIG_HOME: join(root, "xdg/config"),
    XDG_DATA_HOME: join(root, "xdg/data"),
    XDG_CACHE_HOME: join(root, "xdg/cache"),
    XDG_STATE_HOME: join(root, "xdg/state"),
  })
  return env
}

// An `opencode.json` wiring the `mock` provider at the scripted backend.
// LSP and formatters are off so edits never spawn or download servers.
export function providerConfig(baseURL: string, extra: Record<string, unknown> = {}) {
  return {
    $schema: "https://opencode.ai/config.json",
    provider: {
      [PROVIDER]: {
        name: "Scripted",
        options: { apiKey: "test-key", baseURL },
        models: { [MODEL]: { name: "Mock Model", limit: { context: 100000, output: 4096 } } },
      },
    },
    model: `${PROVIDER}/${MODEL}`,
    share: "disabled",
    autoupdate: false,
    lsp: false,
    formatter: false,
    ...extra,
  }
}

export function writeJson(path: string, value: unknown) {
  mkdirSync(dirname(path), { recursive: true })
  writeFileSync(path, JSON.stringify(value, null, 2) + "\n")
}

// Fixed identity and dates make the root commit, and so the project id
// alforria derives from it, identical on every run and machine.
const GIT_ENV = {
  GIT_CONFIG_GLOBAL: "/dev/null",
  GIT_CONFIG_NOSYSTEM: "1",
  GIT_AUTHOR_NAME: "alforria dev",
  GIT_AUTHOR_EMAIL: "dev@alforria.invalid",
  GIT_COMMITTER_NAME: "alforria dev",
  GIT_COMMITTER_EMAIL: "dev@alforria.invalid",
  GIT_AUTHOR_DATE: "2026-01-01T00:00:00Z",
  GIT_COMMITTER_DATE: "2026-01-01T00:00:00Z",
}

function git(dir: string, args: string[]) {
  const run = Bun.spawnSync(["git", ...args], { cwd: dir, env: { ...process.env, ...GIT_ENV }, stderr: "pipe" })
  if (run.exitCode !== 0) throw new Error(`git ${args.join(" ")} in ${dir}: ${run.stderr.toString()}`)
}

// A git repo holding `files`, committed once.
export function gitProject(dir: string, files: Record<string, string>) {
  for (const [path, content] of Object.entries(files)) {
    mkdirSync(dirname(join(dir, path)), { recursive: true })
    writeFileSync(join(dir, path), content)
  }
  git(dir, ["init", "-q", "-b", "main"])
  git(dir, ["add", "-A"])
  git(dir, ["commit", "-q", "-m", "initial commit"])
}

export type Serve = { url: string; port: number; proc: Bun.Subprocess; stop(): Promise<void> }

const HANDSHAKE = /alforria server listening on http:\/\/([^\s:]+):(\d+)/

// Spawn `alforria serve` and resolve once its handshake line names the
// bound port. stdout/stderr go to `log` (and stay drained).
export async function spawnServe(opts: {
  cwd: string
  env: Record<string, string>
  hostname: string
  port: number
  log: string
}): Promise<Serve> {
  mkdirSync(dirname(opts.log), { recursive: true })
  const fd = openSync(opts.log, "w")
  const proc = Bun.spawn([BIN, "serve", "--hostname", opts.hostname, "--port", String(opts.port)], {
    cwd: opts.cwd,
    env: opts.env,
    stdout: "pipe",
    stderr: fd,
  })
  let handshake = (_port: number) => {}
  const bound = new Promise<number>((ok) => (handshake = ok))
  // Keep draining stdout into the log after the handshake.
  const drained = (async () => {
    const decoder = new TextDecoder()
    let buffer = ""
    for await (const chunk of proc.stdout) {
      writeSync(fd, chunk)
      buffer += decoder.decode(chunk, { stream: true })
      const match = HANDSHAKE.exec(buffer)
      if (match) handshake(Number(match[2]))
    }
  })()
  // A cleared timer, not Bun.sleep: a pending sleep keeps the process alive.
  let timer: Timer | undefined
  const failed = Promise.race([
    proc.exited.then((code) => `alforria serve exited with ${code}`),
    new Promise<string>((ok) => (timer = setTimeout(() => ok("no serve handshake within 60s"), 60_000))),
  ])
  const port = await Promise.race([bound, failed])
  clearTimeout(timer)
  if (typeof port === "string") {
    proc.kill("SIGKILL")
    throw new Error(`${port}; see ${opts.log}:\n${await Bun.file(opts.log).text()}`)
  }
  return {
    // The loopback URL works for both 127.0.0.1 and 0.0.0.0 binds.
    url: `http://127.0.0.1:${port}`,
    port,
    proc,
    async stop() {
      proc.kill("SIGINT")
      const timeout = setTimeout(() => proc.kill("SIGKILL"), 5_000)
      await proc.exited
      await drained
      clearTimeout(timeout)
      closeSync(fd)
    },
  }
}

// A v1 client bound to one location directory, like the CLI's client: GET
// carries `?directory=`, other methods the `x-opencode-directory` header.
export class Api {
  constructor(
    readonly base: string,
    readonly directory?: string,
  ) {}

  at(directory: string) {
    return new Api(this.base, directory)
  }

  async request(method: string, path: string, body?: unknown): Promise<unknown> {
    const url = new URL(path, this.base)
    const headers: Record<string, string> = { "content-type": "application/json" }
    if (this.directory) {
      if (method === "GET") url.searchParams.set("directory", this.directory)
      else headers["x-opencode-directory"] = encodeURIComponent(this.directory)
    }
    const response = await fetch(url, { method, headers, body: body === undefined ? undefined : JSON.stringify(body) })
    const text = await response.text()
    if (!response.ok) throw new Error(`${method} ${path}: ${response.status} ${text}`)
    return text ? JSON.parse(text) : undefined
  }

  get<T = any>(path: string) {
    return this.request("GET", path) as Promise<T>
  }

  post<T = any>(path: string, body: unknown = {}) {
    return this.request("POST", path, body) as Promise<T>
  }
}

// One parsed /global/event frame: `{directory?, project?, workspace?,
// payload: {id, type, properties} | {id, type, syncEvent}}`.
export type GlobalFrame = {
  directory?: string
  project?: string
  workspace?: string
  payload: { id: string; type: string; properties?: any; syncEvent?: any }
}

// Reads `/global/event` into `frames`, in arrival order. SSE blocks are
// split on blank lines; `data:` lines are joined and parsed as JSON.
export class EventStream {
  readonly frames: GlobalFrame[] = []
  private listeners = new Set<(frame: GlobalFrame) => void>()
  private abort = new AbortController()

  static async open(base: string): Promise<EventStream> {
    const stream = new EventStream()
    const response = await fetch(new URL("/global/event", base), { signal: stream.abort.signal })
    if (!response.ok || !response.body) throw new Error(`/global/event: ${response.status}`)
    const connected = stream.next((frame) => frame.payload.type === "server.connected", 10_000)
    stream.pump(response.body)
    await connected
    return stream
  }

  private async pump(body: ReadableStream<Uint8Array>) {
    const decoder = new TextDecoder()
    let buffer = ""
    try {
      for await (const chunk of body) {
        buffer += decoder.decode(chunk, { stream: true }).replace(/\r\n/g, "\n")
        let index: number
        while ((index = buffer.indexOf("\n\n")) >= 0) {
          const block = buffer.slice(0, index)
          buffer = buffer.slice(index + 2)
          const data = block
            .split("\n")
            .filter((line) => line.startsWith("data:"))
            .map((line) => line.slice(5).replace(/^ /, ""))
          if (!data.length) continue
          const frame = JSON.parse(data.join("\n")) as GlobalFrame
          this.frames.push(frame)
          for (const listener of this.listeners) listener(frame)
        }
      }
    } catch (error) {
      if (!this.abort.signal.aborted) console.error("/global/event stream failed:", error)
    }
  }

  on(listener: (frame: GlobalFrame) => void) {
    this.listeners.add(listener)
    return () => this.listeners.delete(listener)
  }

  // The first frame from now on matching `match`.
  next(match: (frame: GlobalFrame) => boolean, timeoutMs: number): Promise<GlobalFrame> {
    return new Promise((ok, fail) => {
      const timer = setTimeout(() => {
        off()
        fail(new Error(`timed out after ${timeoutMs}ms waiting for an event`))
      }, timeoutMs)
      const off = this.on((frame) => {
        if (!match(frame)) return
        clearTimeout(timer)
        off()
        ok(frame)
      })
    })
  }

  close() {
    this.abort.abort()
  }
}

export async function waitUntil<T>(
  what: string,
  check: () => Promise<T | undefined | false>,
  timeoutMs = 30_000,
): Promise<T> {
  const deadline = Date.now() + timeoutMs
  while (Date.now() < deadline) {
    const value = await check()
    if (value) return value
    await Bun.sleep(100)
  }
  throw new Error(`timed out waiting for ${what}`)
}

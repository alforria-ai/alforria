// `bun test/pty.e2e.ts [directory]` — walks the PTY protocol the terminal
// column (src/views/focus/terminal.tsx) speaks, against a running server: the
// dev harness (`bun run server`, port 4697) unless ALFORRIA_URL says otherwise.
// Not part of vitest: it needs a live server and spawns real shells, which it
// removes again.
//
// Covers: shells, create, the ticket's header and origin rules, single use,
// replay + the 0x00 meta frame, cursor resume, input, resize, exit (code via
// v2), delete, and the pty.* events on /global/event.

export {} // a module, for top-level await

const BASE = (process.env.ALFORRIA_URL ?? "http://127.0.0.1:4697").replace(/\/$/, "")
const ORIGIN = BASE // same host as the server: always allowed
const EVIL = "https://evil.example"

// Bun's WebSocket takes request headers; the DOM typing does not know that.
const BunWebSocket = WebSocket as unknown as new (url: string, init: { headers: Record<string, string> }) => WebSocket

let failures = 0
function check(ok: unknown, label: string, detail?: unknown) {
  if (ok) console.log(`  ✓ ${label}`)
  else {
    failures++
    console.log(
      `  ✗ ${label}${detail === undefined ? "" : ` — ${typeof detail === "string" ? detail : JSON.stringify(detail)}`}`,
    )
  }
}

async function until<T>(get: () => T | undefined | false, label: string, ms = 10_000): Promise<T> {
  const end = Date.now() + ms
  for (;;) {
    const value = get()
    if (value) return value
    if (Date.now() > end) throw new Error(`timed out: ${label}`)
    await new Promise((r) => setTimeout(r, 25))
  }
}

async function http(method: string, path: string, opts: { body?: unknown; headers?: Record<string, string> } = {}) {
  const res = await fetch(`${BASE}${path}`, {
    method,
    headers: {
      ...(opts.body === undefined ? {} : { "content-type": "application/json" }),
      ...opts.headers,
    },
    body: opts.body === undefined ? undefined : JSON.stringify(opts.body),
  })
  const text = await res.text()
  let body: unknown = text
  try {
    body = text ? JSON.parse(text) : undefined
  } catch {
    /* keep the text */
  }
  return { status: res.status, body: body as any }
}

const q = (directory: string) => `directory=${encodeURIComponent(directory)}`
// CSI, OSC (BEL- or ST-terminated, e.g. systemd's 3008 shell marks), charset picks, CR.
const strip = (s: string) =>
  s.replace(/\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[()][0-9A-B]|\r/g, "")

interface Conn {
  ws: WebSocket
  replay: string // text before the meta frame
  live: string // text after it
  metas: number[]
  cursor: number // client-side offset: meta cursor + live UTF-16 units
  opened: Promise<boolean>
  closed: Promise<CloseEvent>
}

function connect(id: string, directory: string, ticket: string, cursor?: number, origin = ORIGIN): Conn {
  const url = new URL(`/pty/${id}/connect`, BASE.replace(/^http/, "ws"))
  url.searchParams.set("directory", directory)
  url.searchParams.set("ticket", ticket)
  if (cursor !== undefined) url.searchParams.set("cursor", String(cursor))
  const ws = new BunWebSocket(url.href, { headers: { Origin: origin } })
  ws.binaryType = "arraybuffer"
  const conn: Conn = { ws, replay: "", live: "", metas: [], cursor: -1, opened: null!, closed: null! }
  conn.opened = new Promise((resolve) => {
    ws.addEventListener("open", () => resolve(true))
    ws.addEventListener("error", () => resolve(false))
  })
  conn.closed = new Promise((resolve) => ws.addEventListener("close", resolve))
  ws.addEventListener("message", (e: MessageEvent) => {
    if (typeof e.data === "string") {
      if (conn.metas.length) {
        conn.live += e.data
        conn.cursor += e.data.length
      } else conn.replay += e.data
      return
    }
    const bytes = new Uint8Array(e.data as ArrayBuffer)
    if (bytes[0] !== 0) throw new Error(`binary frame without the 0x00 marker: ${bytes[0]}`)
    const meta = JSON.parse(new TextDecoder().decode(bytes.subarray(1))) as { cursor: number }
    conn.metas.push(meta.cursor)
    conn.cursor = meta.cursor
  })
  return conn
}

async function ticket(id: string, directory: string) {
  const res = await http("POST", `/pty/${id}/connect-token?${q(directory)}`, {
    headers: { "x-opencode-ticket": "1", origin: ORIGIN },
  })
  if (res.status !== 200) throw new Error(`connect-token → ${res.status} ${JSON.stringify(res.body)}`)
  return res.body as { ticket: string; expires_in: number }
}

// /global/event, collecting pty.* frames.
const events: { type: string; properties: any }[] = []
const stream = new AbortController()
async function listen() {
  const res = await fetch(`${BASE}/global/event`, { signal: stream.signal })
  const reader = res.body!.getReader()
  const decoder = new TextDecoder()
  let buf = ""
  for (;;) {
    const { done, value } = await reader.read()
    if (done) return
    buf += decoder.decode(value, { stream: true })
    let nl
    while ((nl = buf.indexOf("\n")) >= 0) {
      const line = buf.slice(0, nl).trim()
      buf = buf.slice(nl + 1)
      if (!line.startsWith("data:")) continue
      const frame = JSON.parse(line.slice(5))
      const payload = frame.payload ?? frame
      if (typeof payload.type === "string" && payload.type.startsWith("pty.")) events.push(payload)
    }
  }
}

async function main() {
  const health = await http("GET", "/global/health").catch(() => undefined)
  if (health?.status !== 200) throw new Error(`no server at ${BASE} (start one with \`bun run server\`)`)

  let directory = process.argv[2]
  if (!directory) {
    const projects = (await http("GET", "/project")).body as { worktree: string }[]
    directory = projects.find((p) => p.worktree.includes("/.dev/projects/"))?.worktree ?? projects[0]?.worktree
  }
  if (!directory) throw new Error("no project directory to open a shell in")
  console.log(`server ${BASE}\ndirectory ${directory}\n`)
  void listen().catch(() => {})
  await new Promise((r) => setTimeout(r, 200))

  console.log("shells")
  const shells = await http("GET", "/pty/shells")
  check(
    shells.status === 200 && Array.isArray(shells.body) && shells.body.every((s: any) => "path" in s && "name" in s),
    "GET /pty/shells lists {path, name, acceptable}",
    shells.body,
  )

  console.log("create")
  const created = await http("POST", `/pty?${q(directory)}`, { body: {} })
  const info = created.body
  check(created.status === 200 && info.id?.startsWith("pty_"), "POST /pty → 200 PtyInfo", created)
  check(info.status === "running" && info.cwd === directory, "running, cwd defaults to ?directory", info)
  const id: string = info.id

  console.log("ticket")
  const noHeader = await http("POST", `/pty/${id}/connect-token?${q(directory)}`, { headers: { origin: ORIGIN } })
  check(noHeader.status === 403, "without x-opencode-ticket: 1 → 403", noHeader)
  const evil = await http("POST", `/pty/${id}/connect-token?${q(directory)}`, {
    headers: { "x-opencode-ticket": "1", origin: EVIL },
  })
  check(evil.status === 403, "from a foreign origin → 403", evil)
  const otherDir = await http("POST", `/pty/${id}/connect-token?${q("/")}`, {
    headers: { "x-opencode-ticket": "1", origin: ORIGIN },
  })
  check(otherDir.status === 404, "under another directory → 404 (registries are per directory)", otherDir.status)
  const t1 = await ticket(id, directory)
  check(typeof t1.ticket === "string" && t1.expires_in === 60, "{ticket, expires_in: 60}", t1)

  console.log("connect")
  const foreign = connect(id, directory, t1.ticket, undefined, EVIL)
  check(!(await foreign.opened), "socket from a foreign origin is refused")
  const a = connect(id, directory, t1.ticket)
  check(await a.opened, "the same ticket still opens from an allowed origin (a refused origin does not burn it)")
  await until(() => a.metas.length > 0, "meta frame")
  check(a.metas.length === 1 && a.metas[0]! >= 0, "one 0x00 {cursor} frame after the replay", a.metas)
  const reuse = connect(id, directory, t1.ticket)
  check(!(await reuse.opened), "a used ticket is refused")

  console.log("input")
  a.ws.send("echo hello\n")
  await until(() => /(^|\n)hello\n/.test(strip(a.live)), "hello")
  check(true, 'sent "echo hello\\n", a "hello" line came back')
  a.ws.send(new TextEncoder().encode("echo bin''ary\n"))
  await until(() => /(^|\n)binary\n/.test(strip(a.live)), "binary input")
  check(true, "binary frames are input too")

  console.log("resize")
  const resized = await http("PUT", `/pty/${id}?${q(directory)}`, { body: { size: { rows: 30, cols: 100 } } })
  check(resized.status === 200 && resized.body.id === id, "PUT /pty/{id} {size:{rows,cols}} → 200 PtyInfo", resized)
  a.ws.send("stty size\n")
  await until(() => /(^|\n)30 100\n/.test(strip(a.live)), "stty size")
  check(true, "the shell sees 30 rows × 100 cols")

  console.log("cursor")
  await new Promise((r) => setTimeout(r, 300)) // let the prompt settle
  const mark = a.cursor
  a.ws.close(1000)
  await a.closed
  const b = connect(id, directory, (await ticket(id, directory)).ticket, mark)
  await until(() => b.metas.length > 0, "meta on resume")
  check(b.replay === "" && b.metas[0] === mark, "resume at the client cursor: empty replay, same cursor", {
    replay: b.replay,
    meta: b.metas[0],
    mark,
  })
  b.ws.close(1000)
  await b.closed
  const c = connect(id, directory, (await ticket(id, directory)).ticket, 0)
  await until(() => c.metas.length > 0, "meta on full replay")
  check(
    /(^|\n)hello\n/.test(strip(c.replay)) && c.metas[0] === mark && c.replay.length === mark,
    "cursor 0 replays everything kept; meta = replay length in UTF-16 units",
    { meta: c.metas[0], length: c.replay.length, mark },
  )

  console.log("delete")
  const removed = await http("DELETE", `/pty/${id}?${q(directory)}`)
  check(removed.status === 200 && removed.body === true, "DELETE /pty/{id} → 200 true", removed)
  const closedC = await c.closed
  check(closedC.code === 1000, "the attached socket closes with 1000", closedC.code)
  const gone = await http("GET", `/pty/${id}?${q(directory)}`)
  check(gone.status === 404, "GET afterwards → 404", gone.status)
  const goneTicket = await http("POST", `/pty/${id}/connect-token?${q(directory)}`, {
    headers: { "x-opencode-ticket": "1", origin: ORIGIN },
  })
  check(goneTicket.status === 404, "connect-token afterwards → 404", goneTicket.status)

  console.log("exit")
  const second = (await http("POST", `/pty?${q(directory)}`, { body: {} })).body
  const d = connect(second.id, directory, (await ticket(second.id, directory)).ticket)
  await d.opened
  await until(() => d.metas.length > 0, "meta")
  d.ws.send("exit 3\n")
  const closedD = await d.closed
  check(closedD.code === 1000, "process exit closes the socket with 1000", closedD.code)
  const v1 = await http("GET", `/pty/${second.id}?${q(directory)}`)
  check(v1.status === 404, "v1 GET hides the exited PTY (404)", v1.status)
  const v2 = await http("GET", `/api/pty/${second.id}?location%5Bdirectory%5D=${encodeURIComponent(directory)}`)
  check(
    v2.status === 200 && v2.body.data?.status === "exited" && v2.body.data?.exitCode === 3,
    "v2 GET /api/pty/{id} keeps it: status exited, exitCode 3",
    v2,
  )
  const v1Delete = await http("DELETE", `/pty/${second.id}?${q(directory)}`)
  check(v1Delete.status === 404, "v1 DELETE refuses an exited PTY (404)", v1Delete.status)
  const v2Delete = await http(
    "DELETE",
    `/api/pty/${second.id}?location%5Bdirectory%5D=${encodeURIComponent(directory)}`,
  )
  check(v2Delete.status === 204, "v2 DELETE removes it (204)", v2Delete.status)

  console.log("events")
  await new Promise((r) => setTimeout(r, 300))
  const seen = (type: string, pid: string) =>
    events.some((e) => e.type === type && (e.properties.id ?? e.properties.info?.id) === pid)
  check(
    seen("pty.created", id),
    "pty.created",
    events.map((e) => e.type),
  )
  check(seen("pty.updated", id), "pty.updated")
  check(seen("pty.deleted", id), "pty.deleted")
  const exited = events.find((e) => e.type === "pty.exited" && e.properties.id === second.id)
  check(exited?.properties.exitCode === 3, "pty.exited {id, exitCode: 3}", exited)
  check(seen("pty.deleted", second.id), "pty.deleted after the v2 delete")
}

try {
  await main()
} catch (err) {
  failures++
  console.log(`  ✗ ${err instanceof Error ? err.message : err}`)
} finally {
  stream.abort()
}
console.log(failures ? `\n${failures} failed` : "\nall passed")
process.exit(failures ? 1 : 0)

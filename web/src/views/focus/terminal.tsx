// The terminal column: a shell in the project directory over the server's PTY
// API, drawn by xterm.js. xterm loads on first mount and this module is itself
// lazy, so neither reaches the main bundle.
//
// A shell outlives its column. Unmounting only closes the socket; the PTY id
// stays in `ptys`, and reopening the column replays the output the server
// kept (cursor 0) into a fresh xterm. closeTerminal() is what ends the shell.
//
// Protocol (crates/alforria-server/src/pty): POST /pty spawns the shell. Each
// socket needs a single-use ticket (60 s) from POST /pty/{id}/connect-token,
// asked for with `x-opencode-ticket: 1` from an allowed origin. The socket at
// /pty/{id}/connect?ticket&cursor replays output from `cursor` as text frames,
// then sends one binary frame, 0x00 + {"cursor":N}: the absolute output offset
// in UTF-16 code units. Live text frames follow, so a dropped socket resumes
// where it left off. Input goes up as text frames. The server closes with 1000
// when the process ends and 4404 when the PTY is gone.
import type { FitAddon } from "@xterm/addon-fit"
import type { ITheme, Terminal } from "@xterm/xterm"
import { createEffect, createSignal, on, onCleanup, onMount, Show, type JSX } from "solid-js"
import { ApiError, apiUrl } from "../../api/client"
import "../../ui/terminal.css"

interface Pty {
  id: string
  directory: string
}

interface PtyInfo {
  id: string
  status: "running" | "exited"
  exitCode?: number
}

/** projectID → its shell. Outlives the column; closeTerminal() ends it. */
const ptys = new Map<string, Pty>()
/** Shells being spawned, so two mounts for one project share one PTY. */
const spawning = new Map<string, Promise<Pty>>()
/** PTYs closeTerminal() removed: their sockets close without an exit line. */
const closed = new Set<string>()

async function call<T>(
  method: string,
  path: string,
  query: Record<string, string>,
  init: { body?: unknown; headers?: Record<string, string> } = {},
): Promise<T> {
  const res = await fetch(apiUrl(path, query), {
    method,
    credentials: "same-origin",
    headers: { ...(init.body === undefined ? {} : { "content-type": "application/json" }), ...init.headers },
    body: init.body === undefined ? undefined : JSON.stringify(init.body),
  })
  const text = await res.text()
  let body: unknown = text
  try {
    body = text ? JSON.parse(text) : undefined
  } catch {
    /* not JSON: keep the text */
  }
  if (!res.ok) {
    const detail = typeof body === "object" && body && "message" in body ? String(body.message) : ""
    throw new ApiError(res.status, body, `${method} ${path} → ${res.status}${detail ? `: ${detail}` : ""}`)
  }
  return body as T
}

// PTY registries are per directory, so every call names it.
const pty = {
  create: (directory: string) => call<PtyInfo>("POST", "/pty", { directory }, { body: {} }),
  get: (p: Pty) => call<PtyInfo>("GET", `/pty/${p.id}`, { directory: p.directory }),
  ticket: (p: Pty) =>
    call<{ ticket: string; expires_in: number }>(
      "POST",
      `/pty/${p.id}/connect-token`,
      { directory: p.directory },
      { headers: { "x-opencode-ticket": "1" } },
    ),
  resize: (p: Pty, rows: number, cols: number) =>
    call<PtyInfo>("PUT", `/pty/${p.id}`, { directory: p.directory }, { body: { size: { rows, cols } } }),
  remove: (p: Pty) => call<boolean>("DELETE", `/pty/${p.id}`, { directory: p.directory }),
  // v1 forgets a shell once it exits; the v2 surface still has it, with its code.
  info: (p: Pty) =>
    call<{ data: PtyInfo }>("GET", `/api/pty/${p.id}`, { "location[directory]": p.directory }).then((r) => r.data),
}

/** What became of a shell whose socket the server closed; undefined if the server can't say. */
async function fate(p: Pty): Promise<{ running: boolean; code: number | null } | undefined> {
  const info = await pty.info(p).catch(() => undefined)
  if (info?.status === "running" || info?.status === "exited")
    return { running: info.status === "running", code: info.exitCode ?? null }
  // No v2 answer (gone, or a dev proxy without /api): v1 only knows live shells.
  return pty.get(p).then(
    () => ({ running: true, code: null }),
    (err) => (err instanceof ApiError && err.status === 404 ? { running: false, code: null } : undefined),
  )
}

/** The project's shell: the remembered one, or a new one spawned in `directory`. */
function acquire(projectID: string, directory: string): Promise<{ pty: Pty; resumed: boolean }> {
  const known = ptys.get(projectID)
  if (known?.directory === directory) return Promise.resolve({ pty: known, resumed: true })
  let spawn = spawning.get(projectID)
  if (!spawn) {
    if (known) void pty.remove(known).catch(() => {})
    spawn = pty.create(directory).then((info) => {
      const p = { id: info.id, directory }
      ptys.set(projectID, p)
      return p
    })
    const done = () => spawning.delete(projectID)
    spawn.then(done, done)
    spawning.set(projectID, spawn)
  }
  return spawn.then((p) => ({ pty: p, resumed: false }))
}

/** Ends the project's shell, for when the user closes its column. A no-op if there is none. */
export async function closeTerminal(projectID: string): Promise<void> {
  const p = ptys.get(projectID) ?? (await spawning.get(projectID)?.catch(() => undefined))
  if (!p) return
  if (ptys.get(projectID) === p) ptys.delete(projectID)
  closed.add(p.id)
  // An exited shell is already over (v1 DELETE answers 404 for it).
  await pty.remove(p).catch(() => {})
}

function socketUrl(p: Pty, ticket: string, cursor: number) {
  const url = new URL(apiUrl(`/pty/${p.id}/connect`, { directory: p.directory, ticket, cursor }), location.href)
  url.protocol = url.protocol === "https:" ? "wss:" : "ws:"
  return url.href
}

const loadXterm = () =>
  Promise.all([import("@xterm/xterm"), import("@xterm/addon-fit"), import("@xterm/xterm/css/xterm.css")])

const ANSI = [
  "black",
  "red",
  "green",
  "yellow",
  "blue",
  "magenta",
  "cyan",
  "white",
  "brightBlack",
  "brightRed",
  "brightGreen",
  "brightYellow",
  "brightBlue",
  "brightMagenta",
  "brightCyan",
  "brightWhite",
] as const

/** The xterm theme, from the tokens in effect on `el` (app.css, then terminal.css for ANSI). */
function readTheme(el: HTMLElement): ITheme {
  const style = getComputedStyle(el)
  const v = (name: string) => style.getPropertyValue(`--${name}`).trim()
  const theme: ITheme = {
    background: v("plate"),
    foreground: v("ink"),
    cursor: v("ink"),
    cursorAccent: v("plate"),
    selectionBackground: v("rule-2"),
    selectionInactiveBackground: v("plate-3"),
    scrollbarSliderBackground: v("rule-2"),
    scrollbarSliderHoverBackground: v("ink-3"),
    scrollbarSliderActiveBackground: v("ink-2"),
  }
  for (const key of ANSI) theme[key] = v(`pty-${key.replace(/[A-Z]/, (c) => `-${c.toLowerCase()}`)}`)
  return theme
}

type Phase =
  | { kind: "starting" }
  | { kind: "live" }
  | { kind: "reconnecting"; attempt: number }
  | { kind: "exited"; code: number | null }
  | { kind: "failed"; message: string }

export default function TerminalView(props: {
  projectID: string
  directory: string
  active: boolean
  onExit?: (code: number | null) => void
}): JSX.Element {
  let host!: HTMLDivElement
  const [phase, setPhase] = createSignal<Phase>({ kind: "starting" })

  let term: Terminal | undefined
  let fit: FitAddon | undefined
  let current: Pty | undefined
  let resumed = false // `current` was remembered, not spawned by this mount
  let seen = false // a socket to `current` finished its replay
  let socket: WebSocket | undefined
  let gen = 0 // bumped per attach; stale sockets and callbacks check it
  let cursor = 0 // absolute output offset written to `term`
  let synced = false // the current socket's replay is done
  let replay = { pending: 0 } // replayed writes xterm has yet to parse
  let tries = 0
  let sentSize = ""
  let disposed = false
  let retryTimer: ReturnType<typeof setTimeout> | undefined
  let sizeTimer: ReturnType<typeof setTimeout> | undefined
  let fitTimer: ReturnType<typeof setTimeout> | undefined

  // Spawns (or finds) the shell while xterm loads, then attaches from the
  // start of its retained output.
  async function boot(note?: string) {
    setPhase({ kind: "starting" })
    const acquiring = acquire(props.projectID, props.directory)
    acquiring.catch(() => {}) // reported below, once xterm is up
    if (!term) {
      const mods = await loadXterm().catch(() => undefined)
      if (disposed) return
      if (!mods) return setPhase({ kind: "failed", message: "Could not load the terminal." })
      mount(mods)
    }
    let got: Awaited<typeof acquiring>
    try {
      got = await acquiring
    } catch (err) {
      if (!disposed) setPhase({ kind: "failed", message: `Could not start a shell. ${errorText(err)}` })
      return
    }
    if (disposed) return
    current = got.pty
    resumed = got.resumed
    seen = false
    cursor = 0
    tries = 0
    void attach(note)
  }

  function mount([{ Terminal }, { FitAddon }]: Awaited<ReturnType<typeof loadXterm>>) {
    const t = new Terminal({
      fontFamily: getComputedStyle(host).getPropertyValue("--f-mono").trim() || "monospace",
      fontSize: 12.5,
      lineHeight: 1.2,
      cursorStyle: "block",
      cursorInactiveStyle: "outline",
      cursorBlink: !matchMedia("(prefers-reduced-motion: reduce)").matches,
      scrollback: 10_000,
      theme: readTheme(host),
    })
    const f = new FitAddon()
    t.loadAddon(f)
    t.open(host)
    t.attachCustomKeyEventHandler(keys)
    t.onData(input)
    t.onResize(() => sendSize())
    term = t
    fit = f
    refit()
    requestAnimationFrame(refit)
    if (props.active) t.focus()
  }

  // Opens a socket to `current`, resuming after `cursor`. Cursor 0 replays
  // everything the server kept, which is how a fresh xterm catches up.
  async function attach(note?: string) {
    const p = current
    if (!p || !term) return
    const g = ++gen
    closeSocket()
    clearTimeout(retryTimer)
    let ticket: string
    try {
      ticket = (await pty.ticket(p)).ticket
    } catch (err) {
      if (disposed || g !== gen) return
      if (err instanceof ApiError && err.status === 404) return void ended(p)
      if (err instanceof ApiError && (err.status === 401 || err.status === 403))
        return setPhase({ kind: "failed", message: `The server refused the terminal connection. ${errorText(err)}` })
      return retry()
    }
    if (disposed || g !== gen) return
    if (cursor === 0) {
      // A full replay (or a replay cut short) redraws from a clean screen.
      term.reset()
      if (note) term.write(`\x1b[90m${note}\x1b[0m\r\n`)
    }
    const ws = new WebSocket(socketUrl(p, ticket, cursor))
    ws.binaryType = "arraybuffer"
    socket = ws
    synced = false
    const backlog = (replay = { pending: 0 })
    ws.onopen = () => {
      if (socket !== ws) return
      setPhase({ kind: "live" })
      sentSize = ""
      sendSize(true)
    }
    ws.onmessage = (e: MessageEvent<string | ArrayBuffer>) => {
      if (socket !== ws || !term) return
      const data = e.data
      if (typeof data !== "string") {
        const bytes = new Uint8Array(data)
        if (bytes[0] === 0) meta(new TextDecoder().decode(bytes.subarray(1)))
        return
      }
      if (synced) {
        cursor += data.length
        term.write(data)
      } else {
        backlog.pending++
        term.write(data, () => backlog.pending--)
      }
    }
    ws.onclose = (e) => {
      if (socket !== ws) return
      socket = undefined
      if (disposed) return
      if (e.code === 1000 || e.code === 4404) void ended(p)
      else retry()
    }
  }

  /** The end-of-replay frame: adopt the server's cursor and count from there. */
  function meta(json: string) {
    try {
      const next = (JSON.parse(json) as { cursor?: unknown }).cursor
      if (typeof next === "number" && Number.isSafeInteger(next) && next >= 0) cursor = next
    } catch {
      /* a malformed meta frame still ends the replay */
    }
    synced = true
    seen = true
    tries = 0
  }

  function closeSocket() {
    const ws = socket
    socket = undefined
    if (ws && ws.readyState <= WebSocket.OPEN) ws.close(1000)
  }

  function retry() {
    const delay = Math.min(10_000, 250 * 2 ** tries)
    tries++
    setPhase({ kind: "reconnecting", attempt: tries })
    clearTimeout(retryTimer)
    retryTimer = setTimeout(() => void attach(), delay)
  }

  // The server closed the socket or forgot the PTY: confirm the shell is over,
  // then show how it ended and offer a new one.
  async function ended(p: Pty) {
    if (closed.has(p.id)) return
    const g = gen
    const end = await fate(p)
    if (disposed || g !== gen) return
    // A proxy dropped the socket cleanly, or the server is unreachable: the
    // shell may be fine, and the next ticket (404 if not) settles it.
    if (!end || end.running) return retry()
    if (ptys.get(props.projectID) === p) ptys.delete(props.projectID)
    // The remembered shell ended while the column was closed: start afresh.
    if (resumed && !seen) return void boot("[previous shell ended]")
    term?.write("\x1b[?25l") // hide the cursor: nothing reads input now
    setPhase({ kind: "exited", code: end.code })
    props.onExit?.(end.code)
  }

  /** A new shell in the same column, on a clean screen. */
  function restart() {
    if (current && ptys.get(props.projectID) === current) ptys.delete(props.projectID)
    current = undefined
    void boot()
    term?.focus()
  }

  function input(data: string) {
    if (phase().kind === "exited") {
      if (data === "\r") restart()
      return
    }
    // xterm answers terminal queries (cursor position, device attributes) it
    // finds in replayed output. Those answers are stale, and the shell would
    // read them as typing.
    if (!synced || replay.pending > 0) return
    if (socket?.readyState === WebSocket.OPEN) socket.send(data)
  }

  // Keys the terminal hands back to the page. Everything else belongs to the
  // shell, Escape and Tab included, so Shift+Esc is the way out.
  function keys(e: KeyboardEvent) {
    if (e.type !== "keydown") return true
    const k = e.key.toLowerCase()
    if (e.key === "Escape" && e.shiftKey) {
      e.preventDefault()
      term?.blur()
      return false
    }
    if (e.ctrlKey && e.shiftKey && k === "c") {
      const text = term?.getSelection()
      if (text) void navigator.clipboard?.writeText(text).catch(() => {})
      e.preventDefault()
      return false
    }
    // The browser's own paste event delivers the clipboard.
    if (e.ctrlKey && e.shiftKey && k === "v") return false
    // ⌘K stays the palette; Ctrl+K is the shell's.
    if (e.metaKey && k === "k") return false
    return true
  }

  function refit() {
    // A hidden column measures 0×0, and fitting it would shrink the PTY to one row.
    if (disposed || !term || !fit || !host.clientWidth || !host.clientHeight) return
    fit.fit()
  }

  /** Tells the PTY the terminal's size, once a resize settles. */
  function sendSize(now = false) {
    clearTimeout(sizeTimer)
    sizeTimer = setTimeout(
      () => {
        const p = current
        if (!term || !p || phase().kind !== "live") return
        const size = `${term.rows}x${term.cols}`
        if (size === sentSize) return
        sentSize = size
        pty.resize(p, term.rows, term.cols).catch(() => (sentSize = ""))
      },
      now ? 0 : 100,
    )
  }

  onMount(() => {
    const resize = new ResizeObserver(() => {
      clearTimeout(fitTimer)
      fitTimer = setTimeout(refit, 50)
    })
    resize.observe(host)
    const theme = new MutationObserver(() => {
      if (term) term.options.theme = readTheme(host)
    })
    theme.observe(document.documentElement, { attributes: true, attributeFilter: ["data-theme"] })
    const online = () => {
      if (phase().kind === "reconnecting") void attach()
    }
    window.addEventListener("online", online)
    // On touch screens xterm's gesture layer cancels touchstart (to scroll), so
    // a tap never becomes a click. Focus on a still tap instead: focusing from
    // touchend is what raises the soft keyboard.
    let tap: { x: number; y: number; at: number } | undefined
    const touchStart = (e: TouchEvent) => {
      const t = e.touches[0]
      tap = t && e.touches.length === 1 ? { x: t.clientX, y: t.clientY, at: e.timeStamp } : undefined
    }
    const touchEnd = (e: TouchEvent) => {
      const t = e.changedTouches[0]
      if (tap && t && Math.hypot(t.clientX - tap.x, t.clientY - tap.y) < 10 && e.timeStamp - tap.at < 500) term?.focus()
      tap = undefined
    }
    host.addEventListener("touchstart", touchStart, { passive: true })
    host.addEventListener("touchend", touchEnd)
    onCleanup(() => {
      resize.disconnect()
      theme.disconnect()
      window.removeEventListener("online", online)
      host.removeEventListener("touchstart", touchStart)
      host.removeEventListener("touchend", touchEnd)
    })
    void boot()
  })

  createEffect(
    on(
      () => props.active,
      (active) => active && term?.focus(),
      { defer: true },
    ),
  )

  // The shell stays alive for the next mount; only this view's socket goes.
  onCleanup(() => {
    disposed = true
    clearTimeout(retryTimer)
    clearTimeout(sizeTimer)
    clearTimeout(fitTimer)
    closeSocket()
    term?.dispose()
  })

  const pill = () => {
    const p = phase()
    if (p.kind === "starting") return "Starting"
    if (p.kind === "reconnecting") return `Reconnecting · ${p.attempt}`
  }
  const exited = () => {
    const p = phase()
    return p.kind === "exited" ? p : undefined
  }
  const failed = () => {
    const p = phase()
    return p.kind === "failed" ? p : undefined
  }

  return (
    <div class="pty">
      <div class="pty-screen">
        {/* xterm's own parent: Solid must never re-render its children. */}
        <div class="pty-host" ref={host} />
        <Show when={pill()}>
          {(text) => (
            <span class="pty-state" role="status">
              {text()}
            </span>
          )}
        </Show>
      </div>
      <Show when={exited()}>
        {(p) => (
          <div class="pty-bar" role="status">
            <span class="note">[process exited{p().code === null ? "" : ` · code ${p().code}`}]</span>
            <button class="pty-act" onClick={restart}>
              Restart <kbd>↵</kbd>
            </button>
          </div>
        )}
      </Show>
      <Show when={failed()}>
        {(p) => (
          <div class="pty-bar" role="alert">
            <span class="note prose">{p().message}</span>
            <button class="pty-act" onClick={() => void boot()}>
              Retry
            </button>
          </div>
        )}
      </Show>
    </div>
  )
}

function errorText(err: unknown) {
  return err instanceof Error ? err.message : String(err)
}

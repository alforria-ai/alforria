// Application shell: band, side panel (queue in overview, session list in
// focus), workspace (table / focus columns / settings), status line, and the
// phone tab bar. All navigation state lives in nav/route.ts.
import { createMemo, createSignal, Match, onCleanup, onMount, Show, Switch } from "solid-js"
import { api } from "./api/client"
import { counts, filter, filteredGroups, fleetState, groups, interrupts, setFilter } from "./fleet/fleet"
import { bindKeys } from "./nav/keys"
import {
  closeColumn,
  leaveSettings,
  nav,
  openSession,
  openSettings,
  openTerminal,
  setColumnFile,
  setActiveColumn,
  setNav,
  setView,
  startRouting,
} from "./nav/route"
import { state } from "./store/store"
import { startSync } from "./sync/sync"
import { startPlatform } from "./platform"
import { Band } from "./views/band"
import { Focus } from "./views/focus/focus"
import { MobileTabs } from "./views/mobile-tabs"
import { focusQueue, Queue } from "./views/queue/queue"
import { Rail } from "./views/rail"
import { loadServerInfo } from "./views/server"
import { StatusLine } from "./views/status"
import { SessionTable } from "./views/table"
import { toast, Toasts } from "./views/toast"
import { KeysSheet, Palette } from "./views/palette"
import { Settings } from "./views/settings"

const PHONE = "(max-width: 760px)"

export function App() {
  const [panelOpen, setPanelOpen] = createSignal(true)
  const [phone, setPhone] = createSignal(matchMedia(PHONE).matches)
  const [workspaceWidth, setWorkspaceWidth] = createSignal(1000)
  const [cursor, setCursor] = createSignal<string | null>(null)
  const [palette, setPalette] = createSignal<string | null>(null)
  const [keysOpen, setKeysOpen] = createSignal(false)
  let queueEl: HTMLElement | undefined
  let workspaceEl!: HTMLElement

  const maxColumns = () => (phone() ? 1 : Math.max(1, Math.min(3, Math.floor(workspaceWidth() / 400))))
  // Keyboard row navigation follows what the table shows.
  const rows = createMemo(() => filteredGroups().flatMap((g) => g.rows.map((r) => r.session.id)))

  const open = (id: string, beside = false) => {
    setCursor(id)
    openSession(id, { mode: beside ? "beside" : "replace", maxColumns: maxColumns() })
  }
  const openNextWaiting = () => {
    const first = interrupts()[0]
    if (!first) return toast("Nothing is waiting on you")
    open(first.sessionID)
    requestAnimationFrame(() => {
      const slip = document.querySelector<HTMLElement>(`.col.is-active [data-slip="${first.id}"]`)
      slip?.scrollIntoView({ block: "center" })
      slip?.focus({ preventScroll: true })
    })
  }
  const goQueue = () => {
    setNav("phonePane", "queue")
    if (nav.view !== "overview") setView("overview")
    setPanelOpen(true)
    focusQueue(queueEl)
  }
  const newSession = async (projectID?: string) => {
    const pid = projectID ?? groups()[0]?.project.id ?? state.projectOrder[0]
    const project = pid ? state.projects[pid] : undefined
    if (!project) return toast("No project to start a session in", "error")
    try {
      const s = await api.createSession(project.worktree)
      open(s.id)
      requestAnimationFrame(() => document.querySelector<HTMLTextAreaElement>(".col.is-active textarea")?.focus())
    } catch (err) {
      toast(`Could not create a session: ${err instanceof Error ? err.message : err}`, "error")
    }
  }

  onMount(() => {
    const stopSync = startSync()
    const stopRouting = startRouting()
    void loadServerInfo()
    startPlatform((id) => open(id))
    const mq = matchMedia(PHONE)
    const onPhone = () => setPhone(mq.matches)
    mq.addEventListener("change", onPhone)
    const ro = new ResizeObserver(([e]) => e && setWorkspaceWidth(e.contentRect.width))
    ro.observe(workspaceEl)
    const stopKeys = bindKeys(() => nav.view, {
      palette: () => setPalette(""),
      keys: () => setKeysOpen(true),
      back: () => (nav.view === "settings" ? leaveSettings() : nav.view === "focus" ? setView("overview") : undefined),
      focusView: () => setView("focus"),
      queue: goQueue,
      nextWaiting: openNextWaiting,
      newSession: () => setPalette("New session in "),
      cursor: (d) => {
        const list = rows()
        if (!list.length) return
        const at = list.indexOf(cursor() ?? "")
        const next = list[Math.max(0, Math.min(list.length - 1, at < 0 ? 0 : at + d))]!
        setCursor(next)
        document.querySelector(`tr[data-sid="${next}"]`)?.scrollIntoView({ block: "nearest" })
      },
      openCursor: (beside) => {
        const id = cursor() ?? rows()[0]
        if (id) open(id, beside)
      },
      column: (d) => setActiveColumn((nav.active + d + nav.columns.length) % nav.columns.length),
      stepSession: (d) => {
        const col = nav.columns[nav.active]
        const list = rows()
        const at = col?.kind === "session" ? list.indexOf(col.session) : -1
        const next = list[(at + d + list.length) % list.length]
        if (next) open(next)
      },
      compose: () => document.querySelector<HTMLTextAreaElement>(".col.is-active textarea")?.focus(),
    })
    onCleanup(() => {
      stopSync()
      stopRouting()
      stopKeys()
      mq.removeEventListener("change", onPhone)
      ro.disconnect()
    })
  })

  const phonePane = () => (nav.view === "focus" ? "focus" : nav.view === "settings" ? "settings" : nav.phonePane)

  return (
    <div
      class="app"
      id="app"
      data-view={nav.view}
      data-queue={panelOpen() ? "open" : "closed"}
      data-mobile={phonePane()}
    >
      <Band
        onQueue={goQueue}
        onPalette={() => setPalette("")}
        onNewSession={() => setPalette("New session in ")}
        onSettings={() => openSettings()}
      />
      <main class="main">
        <Show
          when={nav.view === "focus" && !phone()}
          fallback={
            <Queue
              ref={(el) => (queueEl = el)}
              onOpenSession={(id) => open(id)}
              onCollapse={() => setPanelOpen((v) => !v)}
            />
          }
        >
          <Rail onOpen={open} onNextWaiting={openNextWaiting} onCollapse={() => setPanelOpen((v) => !v)} />
        </Show>
        <section class="workspace" id="workspace" aria-label="Workspace" ref={workspaceEl}>
          <Switch>
            <Match when={nav.view === "focus"}>
              <Focus
                maxColumns={maxColumns()}
                onTerminal={(pid) => openTerminal(pid, { maxColumns: maxColumns() })}
                onBack={() => setView("overview")}
                onOpen={open}
                onClose={closeColumn}
              />
            </Match>
            <Match when={nav.view === "settings"}>
              <Settings />
            </Match>
            <Match when={nav.view === "overview"}>
              <div class="pane-head ws-head">
                <h2>Sessions</h2>
                <span class="count">
                  {counts().all} · {state.projectOrder.length} projects
                </span>
                <div class="seg" role="group" aria-label="Filter sessions">
                  {(
                    [
                      ["all", "All", counts().all],
                      ["waiting", "Waiting", counts().waiting],
                      ["working", "Working", counts().working + counts().retry],
                      ["idle", "Idle", counts().idle + counts().fault],
                    ] as const
                  ).map(([id, label, n]) => (
                    <button aria-pressed={filter() === id} onClick={() => setFilter(id)}>
                      <span class="lbl-long">{label}</span>
                      <span class="c">{n}</span>
                    </button>
                  ))}
                </div>
              </div>
              <div class="ws-body">
                <Show
                  when={counts().all > 0}
                  fallback={
                    <div class="q-empty">
                      <h3>{state.conn.state === "live" ? "No sessions yet" : "Connecting…"}</h3>
                      <p>Sessions from every project on this server appear here as they start.</p>
                      <Show when={state.conn.state === "live" && !Object.keys(state.models).length}>
                        <p>No model is connected yet. Sign in with LibertAI or add an API key first.</p>
                        <button class="link-btn primary" onClick={() => openSettings("providers")}>
                          Connect a provider
                        </button>
                      </Show>
                    </div>
                  }
                >
                  <SessionTable cursor={cursor()} onOpen={open} onNewSession={(pid) => void newSession(pid)} />
                </Show>
              </div>
            </Match>
          </Switch>
        </section>
      </main>
      <StatusLine onKeys={() => setKeysOpen(true)} />
      <MobileTabs
        pane={phonePane()}
        waiting={interrupts().length}
        canFocus={nav.columns.length > 0}
        onPane={(p) => {
          if (p === "focus") return setView("focus")
          setNav("phonePane", p)
          setView("overview")
        }}
      />
      <Toasts />
      <Show when={palette() !== null}>
        <Palette
          initial={palette()!}
          actions={{
            openSession: (id) => open(id),
            newSession: (pid) => void newSession(pid),
            openTerminal: (pid) => openTerminal(pid, { maxColumns: maxColumns() }),
            openFile: (sid, path) => {
              open(sid)
              setColumnFile(nav.active, path)
            },
            showKeys: () => setKeysOpen(true),
            close: () => setPalette(null),
          }}
        />
      </Show>
      <Show when={keysOpen()}>
        <KeysSheet close={() => setKeysOpen(false)} />
      </Show>
    </div>
  )
}

export { fleetState }

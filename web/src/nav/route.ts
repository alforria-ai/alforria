// Navigation state and its URL. Every view has a hash route so browser
// Back/Forward work and a reload keeps the open columns:
//   #/                      overview (queue + session table)
//   #/focus/<col>,<col>     focus columns; a col is a session id or term:<projectID>
//   #/settings/<page>       settings
// On phones the same state drives three tabs (queue / sessions / focus); the
// visible view and the highlighted tab can never disagree because both are
// derived from `nav`.
import { createStore, produce } from "solid-js/store"

export type View = "overview" | "focus" | "settings"
export type ColumnTab = "transcript" | "changes" | "files"

export type Column =
  | { key: string; kind: "session"; session: string; tab: ColumnTab; used: number; file?: string }
  | { key: string; kind: "terminal"; project: string; used: number }

export interface Nav {
  view: View
  columns: Column[]
  active: number
  settingsPage: string
  /** The view settings was opened from, for "← Back". */
  back: Exclude<View, "settings">
  /** Phones show one pane: the queue or the table when in overview. */
  phonePane: "queue" | "sessions"
}

export const MAX_COLUMNS = 3

let seq = 0
const key = () => `c${++seq}`

export const [nav, setNav] = createStore<Nav>({
  view: "overview",
  columns: [],
  active: 0,
  settingsPage: "providers",
  back: "overview",
  phonePane: "queue",
})

export function routeHash(n: Nav = nav) {
  if (n.view === "focus" && n.columns.length)
    return `#/focus/${n.columns.map((c) => (c.kind === "session" ? c.session : `term:${c.project}`)).join(",")}`
  if (n.view === "settings") return `#/settings/${n.settingsPage}`
  return "#/"
}

let applying = false
function push() {
  if (applying || typeof history === "undefined") return
  const h = routeHash()
  if (location.hash !== h) history.pushState(null, "", h)
}

/** Parse the current hash into nav state (on boot and on popstate). */
export function applyHash(hash = location.hash) {
  const [, view = "", arg = ""] = (hash || "#/").split("/")
  applying = true
  setNav(
    produce((n) => {
      if (view === "focus" && arg) {
        const prev = n.columns
        n.columns = arg
          .split(",")
          .filter(Boolean)
          .slice(0, MAX_COLUMNS)
          .map((id): Column => {
            if (id.startsWith("term:")) {
              const project = id.slice(5)
              return (
                prev.find((c) => c.kind === "terminal" && c.project === project) ?? {
                  key: key(),
                  kind: "terminal",
                  project,
                  used: Date.now(),
                }
              )
            }
            return (
              prev.find((c) => c.kind === "session" && c.session === id) ?? {
                key: key(),
                kind: "session",
                session: id,
                tab: "transcript",
                used: Date.now(),
              }
            )
          })
        n.active = Math.min(n.active, Math.max(0, n.columns.length - 1))
        n.view = n.columns.length ? "focus" : "overview"
      } else if (view === "settings") {
        if (n.view !== "settings") n.back = n.view
        n.settingsPage = arg || "providers"
        n.view = "settings"
      } else {
        n.view = "overview"
      }
    }),
  )
  applying = false
}

export function setView(view: View) {
  setNav(
    produce((n) => {
      if (view === "settings" && n.view !== "settings") n.back = n.view
      n.view = view === "focus" && !n.columns.length ? "overview" : view
    }),
  )
  push()
}

export function leaveSettings() {
  setView(nav.back)
}

export function openSettings(page?: string) {
  if (page) setNav("settingsPage", page)
  setView("settings")
}

/**
 * Show a session. "replace" swaps it into the active column (the default, so
 * columns never pile up by accident); "beside" adds a column, or reuses the
 * least recently used other column when full. A session already open is
 * focused where it is.
 */
export function openSession(
  session: string,
  opts: { mode?: "replace" | "beside"; tab?: ColumnTab; maxColumns?: number } = {},
) {
  const max = Math.min(MAX_COLUMNS, opts.maxColumns ?? MAX_COLUMNS)
  setNav(
    produce((n) => {
      const at = n.columns.findIndex((c) => c.kind === "session" && c.session === session)
      if (at >= 0) {
        n.active = at
        const col = n.columns[at]!
        if (opts.tab && col.kind === "session") col.tab = opts.tab
      } else {
        const col: Column = { key: key(), kind: "session", session, tab: opts.tab ?? "transcript", used: Date.now() }
        place(n, col, opts.mode ?? "replace", max)
      }
      n.columns[n.active]!.used = Date.now()
      n.view = "focus"
    }),
  )
  push()
}

export function openTerminal(project: string, opts: { maxColumns?: number } = {}) {
  setNav(
    produce((n) => {
      place(
        n,
        { key: key(), kind: "terminal", project, used: Date.now() },
        "beside",
        Math.min(MAX_COLUMNS, opts.maxColumns ?? MAX_COLUMNS),
      )
      n.view = "focus"
    }),
  )
  push()
}

function place(n: Nav, col: Column, mode: "replace" | "beside", max: number) {
  if (!n.columns.length) {
    n.columns.push(col)
    n.active = 0
  } else if (mode === "beside" && n.columns.length < max) {
    n.columns.push(col)
    n.active = n.columns.length - 1
  } else if (mode === "beside") {
    let lru = -1
    n.columns.forEach((c, i) => {
      if (i !== n.active && (lru < 0 || c.used < n.columns[lru]!.used)) lru = i
    })
    n.active = lru < 0 ? n.active : lru
    n.columns[n.active] = col
  } else {
    n.columns[n.active] = col
  }
}

export function closeColumn(i: number) {
  setNav(
    produce((n) => {
      n.columns.splice(i, 1)
      // Closing a column left of the active one shifts it down by one.
      if (i < n.active) n.active--
      n.active = Math.max(0, Math.min(n.active, n.columns.length - 1))
      if (!n.columns.length) n.view = "overview"
    }),
  )
  push()
}

export function closeOtherColumns() {
  setNav(
    produce((n) => {
      n.columns = [n.columns[n.active]!]
      n.active = 0
    }),
  )
  push()
}

export function setActiveColumn(i: number) {
  if (i === nav.active || !nav.columns[i]) return
  setNav("active", i)
  setNav("columns", i, "used", Date.now())
}

export function setColumnTab(i: number, tab: ColumnTab) {
  const col = nav.columns[i]
  if (col?.kind === "session") setNav("columns", i, { ...col, tab })
}

/** Show a file in a column's Files tab (from a tool row or the palette). */
export function setColumnFile(i: number, file: string) {
  const col = nav.columns[i]
  if (col?.kind === "session") setNav("columns", i, { ...col, tab: "files", file })
}

export function startRouting() {
  applyHash()
  if (!location.hash) history.replaceState(null, "", routeHash())
  const onPop = () => applyHash()
  addEventListener("popstate", onPop)
  return () => removeEventListener("popstate", onPop)
}

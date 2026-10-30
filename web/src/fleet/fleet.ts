// Reactive fleet selectors over the store: which sessions to show, in which
// order, grouped by project (or by folder, outside git). Views read these instead of walking the store.
import { createMemo, createRoot, createSignal } from "solid-js"
import type { Project, Session } from "../api/types"
import { state } from "../store/store"
import { basename } from "../ui/format"
import { activity, allInterrupts, contextUse, fleetState, sessionCost, todoProgress, type FleetState } from "./derive"

export function projectName(p: Project) {
  return p.name || basename(p.worktree)
}

/** The server's project for every folder outside git; its worktree is "/". */
export const GLOBAL_PROJECT = "global"

/**
 * Where a session lives for display: its git project, or for a session
 * outside git its own folder, since the "global" project spans them all.
 */
export function placeOf(s: Session) {
  if (s.projectID === GLOBAL_PROJECT)
    return { key: `${GLOBAL_PROJECT}:${s.directory}`, name: basename(s.directory), directory: s.directory }
  const p = state.projects[s.projectID]
  return { key: s.projectID, name: p ? projectName(p) : "", directory: p?.worktree ?? s.directory }
}

/** Every folder a new session can start in: each git project, and each plain folder with a session. */
export function places() {
  const out = new Map<string, { key: string; name: string; directory: string }>()
  for (const id of state.projectOrder) {
    const p = state.projects[id]
    if (p && id !== GLOBAL_PROJECT) out.set(id, { key: id, name: projectName(p), directory: p.worktree })
  }
  const recent = Object.values(state.sessions)
    .filter((s) => s.projectID === GLOBAL_PROJECT && !s.time.archived)
    .sort((a, b) => b.time.updated - a.time.updated)
  for (const s of recent) {
    const place = placeOf(s)
    if (!out.has(place.key)) out.set(place.key, place)
  }
  return [...out.values()]
}

/**
 * A folder to ask server-wide questions in (models, providers, agents): a
 * real place, never the "global" project's "/", which would boot an instance
 * at the root. Undefined (the server's own directory) when there is none yet.
 */
export function anyDirectory(): string | undefined {
  return places()[0]?.directory
}

/**
 * A session's title for display. Subagent sessions arrive titled
 * "<task> (@explore subagent)"; the agent is already shown as a tag, so the
 * suffix only truncates the task text.
 */
export function displayTitle(s: Pick<Session, "title"> | undefined) {
  const t = s?.title?.replace(/\s*\(@[\w-]+ (?:subagent|workflow agent)\)\s*$/, "") ?? ""
  return t || "Untitled session"
}

export interface Row {
  session: Session
  depth: number
}

export interface ProjectGroup {
  /** A project id, or `global:<directory>` for a folder outside git. */
  key: string
  project: Project
  name: string
  /** Where the group's sessions live, and where a new one starts. */
  directory: string
  rows: Row[]
}

/**
 * Sessions shown in the fleet: live ones always, history for a while. The
 * snapshot lists exactly this window (plus whatever is live or open).
 */
export const HISTORY_WINDOW = 3 * 86_400_000

function visible(s: Session, live: Set<string>) {
  if (s.time.archived) return false
  return live.has(s.id) || Date.now() - s.time.updated < HISTORY_WINDOW
}

const memos = createRoot(() => {
  const interrupts = createMemo(() => allInterrupts(state))

  /** Sessions that need a row no matter how old they are. */
  const liveIds = createMemo(() => {
    const ids = new Set<string>()
    for (const [id, st] of Object.entries(state.status)) if (st.type !== "idle") ids.add(id)
    for (const i of interrupts()) ids.add(i.sessionID)
    return ids
  })

  const groups = createMemo<ProjectGroup[]>(() => {
    const live = liveIds()
    const byPlace = new Map<string, Session[]>()
    for (const s of Object.values(state.sessions)) {
      if (!visible(s, live)) continue
      const key = placeOf(s).key
      const list = byPlace.get(key) ?? []
      list.push(s)
      byPlace.set(key, list)
    }
    const group = (project: Project, list: Session[]): ProjectGroup => {
      // Parents newest first; each parent's subagents nested under it.
      const ids = new Set(list.map((s) => s.id))
      const roots: Session[] = []
      const children = new Map<string, Session[]>()
      for (const s of list) {
        if (!s.parentID || !ids.has(s.parentID)) roots.push(s)
        else {
          const siblings = children.get(s.parentID)
          if (siblings) siblings.push(s)
          else children.set(s.parentID, [s])
        }
      }
      roots.sort((a, b) => b.time.created - a.time.created)
      const rows: Row[] = []
      const walk = (s: Session, depth: number) => {
        rows.push({ session: s, depth })
        children
          .get(s.id)
          ?.sort((a, b) => a.time.created - b.time.created)
          .forEach((c) => walk(c, depth + 1))
      }
      roots.forEach((s) => walk(s, 0))
      return { ...placeOf(list[0]!), project, rows }
    }
    const out: ProjectGroup[] = []
    for (const pid of state.projectOrder) {
      const project = state.projects[pid]
      if (!project) continue
      if (pid !== GLOBAL_PROJECT) {
        const list = byPlace.get(pid)
        if (list?.length) out.push(group(project, list))
        continue
      }
      // Folders outside git, in path order.
      const folders = [...byPlace.entries()]
        .filter(([key]) => key.startsWith(`${GLOBAL_PROJECT}:`))
        .sort(([a], [b]) => a.localeCompare(b))
      for (const [, list] of folders) out.push(group(project, list))
    }
    // Order is stable: rows must not move under the cursor as sessions update.
    // Surfacing what's waiting is the queue's and the Waiting filter's job.
    return out
  })

  const counts = createMemo(() => {
    const c: Record<FleetState | "all", number> = { all: 0, waiting: 0, working: 0, retry: 0, fault: 0, idle: 0 }
    for (const g of groups())
      for (const r of g.rows) {
        c.all++
        c[fleetState(state, r.session.id)]++
      }
    return c
  })

  const totals = createMemo(() => {
    let cost = 0
    let tokens = 0
    for (const g of groups())
      for (const r of g.rows) {
        cost += sessionCost(state, r.session.id)
        const t = r.session.tokens
        if (t) tokens += t.input + t.output + t.reasoning + t.cache.read + t.cache.write
      }
    return { cost, tokens }
  })

  // The overview's filter: rows by state, empty projects dropped.
  const [filter, setFilter] = createSignal<TableFilter>("all")
  const matches = (st: FleetState) => {
    const f = filter()
    if (f === "all") return true
    if (f === "working") return st === "working" || st === "retry"
    if (f === "idle") return st === "idle" || st === "fault"
    return st === f
  }
  const filteredGroups = createMemo<ProjectGroup[]>(() =>
    groups()
      .map((g) => ({ ...g, rows: g.rows.filter((r) => matches(fleetState(state, r.session.id))) }))
      .filter((g) => g.rows.length),
  )

  return { interrupts, groups, counts, totals, filter, setFilter, filteredGroups }
})

export type TableFilter = "all" | "waiting" | "working" | "idle"

export const { interrupts, groups, counts, totals, filter, setFilter, filteredGroups } = memos

export { activity, contextUse, fleetState, sessionCost, todoProgress }

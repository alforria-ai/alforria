// Reactive fleet selectors over the store: which sessions to show, in which
// order, grouped by project. Views read these instead of walking the store.
import { createMemo, createRoot } from "solid-js"
import type { Project, Session } from "../api/types"
import { state } from "../store/store"
import { basename } from "../ui/format"
import { activity, allInterrupts, contextUse, fleetState, sessionCost, todoProgress, type FleetState } from "./derive"

export function projectName(p: Project) {
  return p.name || basename(p.worktree)
}

export interface Row {
  session: Session
  depth: number
}

export interface ProjectGroup {
  project: Project
  rows: Row[]
}

/** Sessions shown in the fleet: live ones always, history for a while. */
const HISTORY_WINDOW = 3 * 86_400_000

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
    const byProject = new Map<string, Session[]>()
    for (const s of Object.values(state.sessions)) {
      if (!visible(s, live)) continue
      const list = byProject.get(s.projectID) ?? []
      list.push(s)
      byProject.set(s.projectID, list)
    }
    const out: ProjectGroup[] = []
    for (const pid of state.projectOrder) {
      const project = state.projects[pid]
      const list = byProject.get(pid)
      if (!project || !list?.length) continue
      // Parents newest first; each parent's subagents nested under it.
      const ids = new Set(list.map((s) => s.id))
      const roots = list
        .filter((s) => !s.parentID || !ids.has(s.parentID))
        .sort((a, b) => b.time.updated - a.time.updated)
      const rows: Row[] = []
      const walk = (s: Session, depth: number) => {
        rows.push({ session: s, depth })
        list
          .filter((c) => c.parentID === s.id)
          .sort((a, b) => a.time.created - b.time.created)
          .forEach((c) => walk(c, depth + 1))
      }
      roots.forEach((s) => walk(s, 0))
      out.push({ project, rows })
    }
    // Projects with something waiting or working float to the top.
    const weight = (g: ProjectGroup) =>
      g.rows.some((r) => fleetState(state, r.session.id) === "waiting")
        ? 2
        : g.rows.some((r) => fleetState(state, r.session.id) === "working")
          ? 1
          : 0
    return out.sort((a, b) => weight(b) - weight(a))
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

  return { interrupts, groups, counts, totals }
})

export const { interrupts, groups, counts, totals } = memos

export { activity, contextUse, fleetState, sessionCost, todoProgress }

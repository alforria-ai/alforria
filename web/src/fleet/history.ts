// Session history: every session the server keeps, by folder, however old.
// The fleet shows only the last few days; these helpers sort, filter and
// group the full list the history dialog walks from the server.
import type { GlobalSession } from "../api/client"
import { age, basename } from "../ui/format"
import { GLOBAL_PROJECT } from "./fleet"

/** A folder sessions ran in: a git project, or a plain folder outside git. */
export interface Folder {
  /** The fleet's group key: a project id, or `global:<directory>`. */
  key: string
  name: string
  directory: string
}

/** Where a listed session lives, keyed like the fleet's groups (`placeOf`). */
export function folderOf(s: GlobalSession): Folder {
  if (s.projectID === GLOBAL_PROJECT)
    return { key: `${GLOBAL_PROJECT}:${s.directory}`, name: basename(s.directory), directory: s.directory }
  const worktree = s.project?.worktree ?? s.directory
  return { key: s.projectID, name: s.project?.name || basename(worktree), directory: worktree }
}

/** Every folder in the list, most recently used first, with its session count. */
export function historyFolders(list: GlobalSession[]): (Folder & { count: number })[] {
  const out = new Map<string, Folder & { count: number; last: number }>()
  for (const s of list) {
    const f = folderOf(s)
    const seen = out.get(f.key)
    if (seen) {
      seen.count++
      seen.last = Math.max(seen.last, s.time.updated)
    } else out.set(f.key, { ...f, count: 1, last: s.time.updated })
  }
  return [...out.values()].sort((a, b) => b.last - a.last).map(({ last: _, ...f }) => f)
}

/**
 * Sessions in `folder` (a key; "" for every folder) whose title holds every
 * word of `query`. Across folders the folder's name matches too. Newest first.
 */
export function filterHistory(list: GlobalSession[], folder: string, query: string) {
  const words = query.toLowerCase().split(/\s+/).filter(Boolean)
  return list
    .filter((s) => {
      const f = folderOf(s)
      if (folder && f.key !== folder) return false
      if (!words.length) return true
      const text = `${s.title} ${folder ? "" : f.name}`.toLowerCase()
      return words.every((w) => text.includes(w))
    })
    .sort((a, b) => b.time.updated - a.time.updated || (a.id < b.id ? 1 : -1))
}

const MONTHS = [
  "January",
  "February",
  "March",
  "April",
  "May",
  "June",
  "July",
  "August",
  "September",
  "October",
  "November",
  "December",
]
const DAY = 86_400_000

/** The heading a session sorts under by when it was last used: recent spans, then calendar months. */
export function bucketOf(updated: number, now: number) {
  const ago = now - updated
  if (ago < DAY) return "Today"
  if (ago < 7 * DAY) return "Past week"
  if (ago < 30 * DAY) return "Past month"
  const d = new Date(updated)
  return `${MONTHS[d.getMonth()]} ${d.getFullYear()}`
}

/** When a session was last used: "5h ago" this week, a date ("Sep 14") before. */
export function lastUsed(updated: number, now: number) {
  if (now - updated < 7 * DAY) return `${age(now - updated)} ago`
  const d = new Date(updated)
  return `${MONTHS[d.getMonth()]!.slice(0, 3)} ${d.getDate()}`
}

/** Consecutive runs of the (sorted) list under one heading each. */
export function bucketed<T extends { time: { updated: number } }>(list: T[], now: number) {
  const out: { label: string; items: T[] }[] = []
  for (const s of list) {
    const label = bucketOf(s.time.updated, now)
    const last = out[out.length - 1]
    if (last?.label === label) last.items.push(s)
    else out.push({ label, items: [s] })
  }
  return out
}

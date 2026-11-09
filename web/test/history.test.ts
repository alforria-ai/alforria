// Session history: folders and filters over the server's full session list,
// and an old session opened from it rejoining the fleet while it is open.
import { beforeEach, describe, expect, test } from "vitest"
import type { GlobalSession } from "../src/api/client"
import type { Project, Session } from "../src/api/types"

const { bucketOf, bucketed, filterHistory, folderOf, historyFolders, lastUsed } = await import("../src/fleet/history")
const { filteredGroups, groups, HISTORY_WINDOW, listedGroups, setFilter, subagentSummary } =
  await import("../src/fleet/fleet")
const { closeColumn, nav, openSession, setNav } = await import("../src/nav/route")
const { setAllSubagentsCollapsed, setSubagentsCollapsed, subagentsCollapsed, toggleSubagents } =
  await import("../src/nav/collapse")
const { setState } = await import("../src/store/store")
const { emptyState } = await import("../src/store/state")

const DAY = 86_400_000
const NOW = new Date(2026, 10, 9, 12).getTime()

const listed = (id: string, updated: number, extra: Partial<GlobalSession> = {}): GlobalSession =>
  ({
    id,
    slug: id,
    projectID: "prj_app",
    directory: "/repos/app",
    title: id,
    version: "1",
    time: { created: updated, updated },
    project: { id: "prj_app", name: "app", worktree: "/repos/app" },
    ...extra,
  }) as GlobalSession
const plain = (id: string, directory: string, updated: number) =>
  listed(id, updated, { projectID: "global", directory, project: { id: "global", worktree: "/" } })

describe("folders", () => {
  test("a git project is one folder across its subfolders; plain folders are their own", () => {
    expect(folderOf(listed("a", 1, { directory: "/repos/app/web" }))).toEqual({
      key: "prj_app",
      name: "app",
      directory: "/repos/app",
    })
    expect(folderOf(plain("b", "/tmp/notes", 1))).toEqual({
      key: "global:/tmp/notes",
      name: "notes",
      directory: "/tmp/notes",
    })
    // A project without a name is named after its worktree.
    expect(folderOf(listed("c", 1, { project: { id: "prj_app", worktree: "/repos/app" } })).name).toBe("app")
  })

  test("every folder once, most recently used first, with counts", () => {
    const list = [
      listed("a1", NOW - 40 * DAY),
      plain("n1", "/tmp/notes", NOW - 2 * DAY),
      listed("a2", NOW - 50 * DAY, { directory: "/repos/app/web" }),
      plain("o1", "/srv/old", NOW - 400 * DAY),
    ]
    expect(historyFolders(list).map((f) => [f.key, f.count])).toEqual([
      ["global:/tmp/notes", 1],
      ["prj_app", 2],
      ["global:/srv/old", 1],
    ])
  })
})

describe("filterHistory", () => {
  const list = [
    listed("old", NOW - 90 * DAY, { title: "Fix the login redirect" }),
    plain("note", "/tmp/notes", NOW - 30 * DAY),
    listed("new", NOW - DAY, { title: "Add cursor pagination" }),
  ]

  test("newest first, one folder or all", () => {
    expect(filterHistory(list, "", "").map((s) => s.id)).toEqual(["new", "note", "old"])
    expect(filterHistory(list, "prj_app", "").map((s) => s.id)).toEqual(["new", "old"])
    expect(filterHistory(list, "global:/tmp/notes", "").map((s) => s.id)).toEqual(["note"])
  })

  test("every word must match the title, in any case", () => {
    expect(filterHistory(list, "", "LOGIN fix").map((s) => s.id)).toEqual(["old"])
    expect(filterHistory(list, "", "login pagination")).toEqual([])
  })

  test("across folders the folder's name matches; inside one it doesn't", () => {
    expect(filterHistory(list, "", "notes").map((s) => s.id)).toEqual(["note"])
    expect(filterHistory(list, "global:/tmp/notes", "notes")).toEqual([])
  })
})

describe("headings and dates", () => {
  test("recent spans, then calendar months", () => {
    expect(bucketOf(NOW - 3_600_000, NOW)).toBe("Today")
    expect(bucketOf(NOW - 3 * DAY, NOW)).toBe("Past week")
    expect(bucketOf(NOW - 20 * DAY, NOW)).toBe("Past month")
    expect(bucketOf(new Date(2026, 8, 14).getTime(), NOW)).toBe("September 2026")
    expect(bucketOf(new Date(2025, 11, 31).getTime(), NOW)).toBe("December 2025")
  })

  test("last used: an age this week, a date before", () => {
    expect(lastUsed(NOW - 5 * 3_600_000, NOW)).toBe("5h ago")
    expect(lastUsed(new Date(2026, 8, 14, 9).getTime(), NOW)).toBe("Sep 14")
  })

  test("a sorted list runs under one heading per bucket", () => {
    const list = [listed("a", NOW - 1000), listed("b", NOW - 2000), listed("c", NOW - 60 * DAY)]
    expect(bucketed(list, NOW).map((g) => [g.label, g.items.map((s) => s.id)])).toEqual([
      ["Today", ["a", "b"]],
      [bucketOf(NOW - 60 * DAY, NOW), ["c"]],
    ])
  })
})

describe("reviving a session", () => {
  const project: Project = {
    id: "prj",
    worktree: "/a",
    vcs: "git",
    time: { created: 1, updated: 1 },
    sandboxes: [],
  }
  const session = (id: string, updated: number): Session =>
    ({
      id,
      slug: id,
      projectID: "prj",
      directory: "/a",
      title: id,
      version: "1",
      time: { created: 1, updated },
    }) as Session
  const shown = () => groups().flatMap((g) => g.rows.map((r) => r.session.id))

  beforeEach(() => {
    setNav({ view: "overview", columns: [], active: 0 })
    const d = emptyState()
    d.projects = { prj: project }
    d.projectOrder = ["prj"]
    d.sessions = {
      recent: session("recent", Date.now()),
      old: session("old", Date.now() - HISTORY_WINDOW - DAY),
    }
    setState(d)
  })

  test("an old session is listed while it is open in a column, and drops out once closed", () => {
    expect(shown()).toEqual(["recent"])
    openSession("old", { mode: "replace", maxColumns: 1 })
    expect(nav.columns.map((c) => c.kind === "session" && c.session)).toEqual(["old"])
    expect(shown().sort()).toEqual(["old", "recent"])
    closeColumn(0)
    expect(shown()).toEqual(["recent"])
  })

  test("used again, it is back for good", () => {
    openSession("old", { mode: "replace", maxColumns: 1 })
    setState("sessions", "old", "time", "updated", Date.now())
    closeColumn(0)
    expect(shown().sort()).toEqual(["old", "recent"])
  })
})

describe("collapsing subagents", () => {
  const at = Date.now()
  const s = (id: string, parentID?: string, created = 1): Session =>
    ({
      id,
      slug: id,
      projectID: "prj",
      directory: "/a",
      title: id,
      version: "1",
      parentID,
      time: { created, updated: at },
    }) as Session
  const ids = (gs: { rows: { session: { id: string } }[] }[]) => gs.flatMap((g) => g.rows.map((r) => r.session.id))

  beforeEach(() => {
    setNav({ view: "overview", columns: [], active: 0 })
    setAllSubagentsCollapsed(false)
    setFilter("all")
    const d = emptyState()
    d.projects = {
      prj: { id: "prj", worktree: "/a", vcs: "git", time: { created: 1, updated: 1 }, sandboxes: [] },
    }
    d.projectOrder = ["prj"]
    // parent → a → a1 (a subagent's own subagent), parent → b; and a lone session.
    d.sessions = {
      parent: s("parent", undefined, 2),
      a: s("a", "parent", 3),
      a1: s("a1", "a", 4),
      b: s("b", "parent", 5),
      lone: s("lone", undefined, 1),
    }
    d.status = { b: { type: "busy" } } as typeof d.status
    setState(d)
  })

  test("every row knows the subagents under it, at any depth", () => {
    const rows = groups()[0]!.all
    expect(Object.fromEntries(rows.map((r) => [r.session.id, r.subagents]))).toEqual({
      parent: ["a", "a1", "b"],
      a: ["a1"],
      a1: [],
      b: [],
      lone: [],
    })
  })

  test("collapsing a parent hides everything under it; counts still include them", () => {
    setSubagentsCollapsed("parent", true)
    expect(ids(listedGroups())).toEqual(["parent", "lone"])
    expect(ids(filteredGroups())).toEqual(["parent", "lone"])
    expect(groups()[0]!.all).toHaveLength(5)
    expect(subagentSummary(groups()[0]!.all[0]!.subagents).text).toBe("3 subagents · 1 working")
    toggleSubagents("parent")
    expect(ids(listedGroups())).toEqual(["parent", "a", "a1", "b", "lone"])
  })

  test("a nested parent folds on its own", () => {
    setSubagentsCollapsed("a", true)
    expect(ids(listedGroups())).toEqual(["parent", "a", "b", "lone"])
  })

  test("a state filter shows every match, collapsed or not", () => {
    setSubagentsCollapsed("parent", true)
    setFilter("working")
    expect(ids(filteredGroups())).toEqual(["b"])
  })

  test("a subagent open in a column stays listed", () => {
    setSubagentsCollapsed("parent", true)
    openSession("b", { mode: "replace", maxColumns: 1 })
    expect(ids(listedGroups())).toEqual(["parent", "b", "lone"])
  })

  test("hide all sets the default for every parent, new ones included; one can still be shown", () => {
    setAllSubagentsCollapsed(true)
    expect(subagentsCollapsed("parent")).toBe(true)
    expect(subagentsCollapsed("a-parent-from-later")).toBe(true)
    setSubagentsCollapsed("parent", false)
    expect(ids(listedGroups())).toEqual(["parent", "a", "b", "lone"])
    setAllSubagentsCollapsed(false)
    expect(ids(listedGroups())).toEqual(["parent", "a", "a1", "b", "lone"])
  })
})

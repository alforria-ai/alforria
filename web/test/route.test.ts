import { beforeEach, describe, expect, test } from "vitest"
import {
  applyHash,
  closeColumn,
  closeOtherColumns,
  leaveSettings,
  nav,
  openSession,
  openSettings,
  routeHash,
  setActiveColumn,
  setNav,
  setView,
} from "../src/nav/route"

const sessions = () => nav.columns.map((c) => (c.kind === "session" ? c.session : `term:${c.project}`))

beforeEach(() => {
  setNav({ view: "overview", columns: [], active: 0, settingsPage: "providers", back: "overview", phonePane: "queue" })
})

describe("open semantics", () => {
  test("opening replaces the active column by default", () => {
    openSession("a")
    openSession("b")
    expect(sessions()).toEqual(["b"])
    expect(nav.view).toBe("focus")
  })

  test("beside adds a column, up to the width limit, then reuses the least recent other", () => {
    openSession("a")
    openSession("b", { mode: "beside", maxColumns: 2 })
    expect(sessions()).toEqual(["a", "b"])
    openSession("c", { mode: "beside", maxColumns: 2 })
    // "b" is active, so the other column ("a") is replaced.
    expect(sessions()).toEqual(["c", "b"])
    expect(nav.active).toBe(0)
  })

  test("an already open session is focused where it is, not duplicated", () => {
    openSession("a")
    openSession("b", { mode: "beside" })
    openSession("a")
    expect(sessions()).toEqual(["a", "b"])
    expect(nav.active).toBe(0)
  })

  test("closing the last column returns to the overview", () => {
    openSession("a")
    closeColumn(0)
    expect(nav.view).toBe("overview")
    expect(nav.columns).toEqual([])
  })

  test("close others keeps only the active column", () => {
    openSession("a")
    openSession("b", { mode: "beside" })
    openSession("c", { mode: "beside" })
    setActiveColumn(1)
    closeOtherColumns()
    expect(sessions()).toEqual(["b"])
  })
})

describe("hash routes", () => {
  test("round-trip focus columns, including terminals", () => {
    applyHash("#/focus/s1,term:prj,s2")
    expect(nav.view).toBe("focus")
    expect(sessions()).toEqual(["s1", "term:prj", "s2"])
    expect(routeHash()).toBe("#/focus/s1,term:prj,s2")
  })

  test("existing columns keep their state when the hash re-applies", () => {
    openSession("s1", { tab: "changes" })
    const key = nav.columns[0]!.key
    applyHash("#/focus/s1")
    expect(nav.columns[0]!.key).toBe(key)
    expect(nav.columns[0]!.kind === "session" && nav.columns[0]!.tab).toBe("changes")
  })

  test("settings remembers where it was opened from", () => {
    openSession("s1")
    openSettings("permissions")
    expect(routeHash()).toBe("#/settings/permissions")
    leaveSettings()
    expect(nav.view).toBe("focus")
  })

  test("focus with no columns falls back to the overview", () => {
    setView("focus")
    expect(nav.view).toBe("overview")
    applyHash("#/focus/")
    expect(nav.view).toBe("overview")
  })
})

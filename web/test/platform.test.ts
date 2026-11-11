// OS notifications: one per session and kind, whatever the sessions are called.
import { describe, expect, test, vi } from "vitest"
import type { Session } from "../src/api/types"

const raised: { title: string; tag?: string }[] = []
class FakeNotification {
  static permission = "granted"
  onclick: (() => void) | null = null
  constructor(title: string, options?: { tag?: string }) {
    raised.push({ title, tag: options?.tag })
  }
  close() {}
}
vi.stubGlobal("window", { focus() {} })
vi.stubGlobal("localStorage", { getItem: () => "on", setItem() {} })
vi.stubGlobal("Notification", FakeNotification)
vi.stubGlobal("document", { hidden: true, visibilityState: "hidden", title: "", addEventListener() {} })

const { setState } = await import("../src/store/store")
const { startPlatform } = await import("../src/platform")

const session = (id: string): Session =>
  ({
    id,
    slug: id,
    projectID: "prj",
    directory: "/a",
    title: "Fix the build",
    version: "1",
    time: { created: 1, updated: 1 },
  }) as Session

describe("notifications", () => {
  test("two sessions with the same title don't replace each other", async () => {
    setState("sessions", { ses_a: session("ses_a"), ses_b: session("ses_b") })
    setState("status", { ses_a: { type: "busy" }, ses_b: { type: "busy" } })
    setState("synced", true)
    startPlatform(() => {})
    await Promise.resolve()
    setState("status", { ses_a: { type: "idle" }, ses_b: { type: "idle" } })
    await Promise.resolve()
    expect(raised.map((n) => n.title)).toEqual(["Fix the build finished", "Fix the build finished"])
    expect(new Set(raised.map((n) => n.tag)).size).toBe(2)
    expect(raised.every((n) => n.tag?.startsWith("ses_"))).toBe(true)
  })
})

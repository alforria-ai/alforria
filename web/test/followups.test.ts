// The follow-up queue against a mocked API: an item goes out only once the
// one before it has started and its turn has ended.
import { afterEach, beforeEach, describe, expect, test, vi } from "vitest"
import type { Draft } from "../src/views/focus/prompt"

const api = vi.hoisted(() => ({ promptAsync: vi.fn(async (_id: string, _body: unknown) => {}) }))
vi.mock("../src/api/client", async (original) => ({
  ...(await original<typeof import("../src/api/client")>()),
  api,
}))
vi.mock("../src/views/toast", () => ({ toast: vi.fn() }))

const { setState } = await import("../src/store/store")
const { emptyState } = await import("../src/store/state")
const { enqueue, queue, START_TIMEOUT } = await import("../src/views/focus/followups")

const body = (text: string): Draft => ({ text, files: [], attachments: [], directory: "/a", agent: "build" })
const sent = () => api.promptAsync.mock.calls.map((c) => (c[1] as { parts: { text: string }[] }).parts[0]!.text)
const user = (id: string, sessionID: string) => ({ id, sessionID, role: "user", time: { created: 1 } }) as never

let n = 0
let sid = ""
beforeEach(() => {
  vi.useFakeTimers()
  setState(emptyState())
  api.promptAsync.mockClear()
  sid = `ses_${n++}`
})
afterEach(() => vi.useRealTimers())

describe("follow-up queue", () => {
  test("the next item waits for the one before it to start, however slow, and to finish", async () => {
    enqueue(sid, body("one"))
    enqueue(sid, body("two"))
    await vi.advanceTimersByTimeAsync(0)
    expect(sent()).toEqual(["one"])
    // The instance is still booting: accepted, but nothing has moved yet.
    await vi.advanceTimersByTimeAsync(10_000)
    expect(sent()).toEqual(["one"])
    setState("status", sid, { type: "busy" })
    await vi.advanceTimersByTimeAsync(10_000)
    expect(sent()).toEqual(["one"])
    setState("status", sid, { type: "idle" })
    await vi.advanceTimersByTimeAsync(200)
    expect(sent()).toEqual(["one", "two"])
  })

  test("a turn that started and ended between two looks still counts as started", async () => {
    enqueue(sid, body("one"))
    enqueue(sid, body("two"))
    await vi.advanceTimersByTimeAsync(0)
    setState("message", "msg_1", user("msg_1", sid))
    setState("messages", sid, ["msg_1"])
    await vi.advanceTimersByTimeAsync(200)
    expect(sent()).toEqual(["one", "two"])
  })

  test("a prompt that never shows up doesn't hold the queue forever", async () => {
    enqueue(sid, body("one"))
    enqueue(sid, body("two"))
    await vi.advanceTimersByTimeAsync(START_TIMEOUT - 1000)
    expect(sent()).toEqual(["one"])
    await vi.advanceTimersByTimeAsync(2000)
    expect(sent()).toEqual(["one", "two"])
    expect(queue.items[sid]).toEqual([])
  })
})

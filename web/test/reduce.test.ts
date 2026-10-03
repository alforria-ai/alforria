import { describe, expect, test } from "vitest"
import type { GlobalEvent, PermissionRequest, Session } from "../src/api/types"
import { activity, allInterrupts, fleetState, todoProgress } from "../src/fleet/derive"
import { applyMessages, applySnapshot, reduce } from "../src/store/reduce"
import { emptyState, insertSorted } from "../src/store/state"

const frame = (type: string, properties: unknown, directory = "/p"): GlobalEvent =>
  ({ directory, payload: { id: `evt_${Math.random()}`, type, properties } }) as GlobalEvent

const session = (id: string, extra: Partial<Session> = {}): Session =>
  ({
    id,
    slug: id,
    projectID: "prj",
    directory: "/p",
    title: id,
    version: "1",
    time: { created: 1, updated: 1 },
    ...extra,
  }) as Session

const permission = (id: string, sessionID: string): PermissionRequest => ({
  id,
  sessionID,
  permission: "bash",
  patterns: ["git push origin main"],
  metadata: {},
  always: ["git push *"],
})

describe("insertSorted", () => {
  test("keeps ids ascending and unique", () => {
    const list: string[] = []
    for (const id of ["prt_03", "prt_01", "prt_02", "prt_01"]) insertSorted(list, id)
    expect(list).toEqual(["prt_01", "prt_02", "prt_03"])
  })
})

describe("reduce", () => {
  test("streams a text part through updated + deltas", () => {
    const s = emptyState()
    reduce(
      s,
      frame("message.updated", {
        sessionID: "ses",
        info: {
          id: "msg_1",
          sessionID: "ses",
          role: "assistant",
          time: { created: 1 },
          tokens: { input: 0, output: 0, reasoning: 0, cache: { read: 0, write: 0 } },
          cost: 0,
        },
      }),
    )
    reduce(
      s,
      frame("message.part.updated", {
        sessionID: "ses",
        time: 1,
        part: { id: "prt_1", sessionID: "ses", messageID: "msg_1", type: "text", text: "" },
      }),
    )
    reduce(
      s,
      frame("message.part.delta", {
        sessionID: "ses",
        messageID: "msg_1",
        partID: "prt_1",
        field: "text",
        delta: "Hel",
      }),
    )
    reduce(
      s,
      frame("message.part.delta", {
        sessionID: "ses",
        messageID: "msg_1",
        partID: "prt_1",
        field: "text",
        delta: "lo",
      }),
    )
    expect(s.messages.ses).toEqual(["msg_1"])
    expect(s.parts.msg_1).toEqual(["prt_1"])
    expect((s.part.prt_1 as { text: string }).text).toBe("Hello")
  })

  test("a delta for an unknown part is dropped, not invented", () => {
    const s = emptyState()
    reduce(
      s,
      frame("message.part.delta", { sessionID: "ses", messageID: "m", partID: "nope", field: "text", delta: "x" }),
    )
    expect(s.part).toEqual({})
  })

  test("permission asked then replied leaves nothing waiting", () => {
    const s = emptyState()
    s.sessions.ses = session("ses")
    reduce(s, frame("permission.asked", permission("per_1", "ses")))
    expect(fleetState(s, "ses")).toBe("waiting")
    expect(allInterrupts(s).map((i) => i.id)).toEqual(["per_1"])
    reduce(s, frame("permission.replied", { sessionID: "ses", requestID: "per_1", reply: "once" }))
    expect(allInterrupts(s)).toEqual([])
  })

  test("session.deleted drops its transcript and pending requests", () => {
    const s = emptyState()
    s.sessions.ses = session("ses")
    reduce(s, frame("permission.asked", permission("per_1", "ses")))
    applyMessages(s, "ses", [
      {
        info: {
          id: "msg_1",
          sessionID: "ses",
          role: "user",
          time: { created: 1 },
          agent: "build",
          model: { providerID: "p", modelID: "m" },
        },
        parts: [{ id: "prt_1", sessionID: "ses", messageID: "msg_1", type: "text", text: "hi" }],
      },
    ])
    reduce(s, frame("session.deleted", { sessionID: "ses", info: session("ses") }))
    expect(s.sessions).toEqual({})
    expect(s.part).toEqual({})
    expect(s.permissions).toEqual({})
  })

  test("sync frames and unknown types are ignored", () => {
    const s = emptyState()
    reduce(s, frame("sync", { anything: true }))
    reduce(s, frame("server.heartbeat", {}))
    expect(s.sessions).toEqual({})
    expect(s.conn.lastFrame).toBeGreaterThan(0)
  })
})

describe("applySnapshot", () => {
  test("is authoritative: requests answered while offline disappear", () => {
    const s = emptyState()
    s.permissions.per_old = permission("per_old", "ses")
    applySnapshot(
      s,
      [{ id: "prj", worktree: "/p", time: { created: 1, updated: 1 }, sandboxes: [] }],
      [
        {
          directory: "/p",
          sessions: [session("ses")],
          status: { ses: { type: "busy" } },
          permissions: [],
          questions: [],
        },
      ],
    )
    expect(s.permissions).toEqual({})
    expect(fleetState(s, "ses")).toBe("working")
  })
})

describe("derive", () => {
  test("activity names the running tool and its target", () => {
    const s = emptyState()
    s.sessions.ses = session("ses")
    s.status.ses = { type: "busy" }
    applyMessages(s, "ses", [
      {
        info: {
          id: "msg_1",
          sessionID: "ses",
          role: "assistant",
          time: { created: 1 },
          parentID: "x",
          modelID: "m",
          providerID: "p",
          mode: "build",
          agent: "build",
          path: { cwd: "/p", root: "/p" },
          cost: 0.1,
          tokens: { input: 10, output: 5, reasoning: 0, cache: { read: 0, write: 0 } },
        },
        parts: [
          {
            id: "prt_1",
            sessionID: "ses",
            messageID: "msg_1",
            type: "tool",
            callID: "c",
            tool: "bash",
            state: { status: "running", input: { command: "cargo test" }, time: { start: 1 } },
          },
        ],
      },
    ])
    expect(activity(s, "ses")).toEqual({ kind: "bash", text: "cargo test", prose: false })
  })

  test("todo progress ignores cancelled items", () => {
    const s = emptyState()
    s.todos.ses = [
      { content: "a", status: "completed", priority: "high" },
      { content: "b", status: "in_progress", priority: "high" },
      { content: "c", status: "cancelled", priority: "low" },
    ]
    expect(todoProgress(s, "ses")).toEqual([1, 2])
  })

  test("an aborted turn is idle, a provider error is a fault", () => {
    const s = emptyState()
    s.sessions.ses = session("ses")
    const base = {
      id: "msg_1",
      sessionID: "ses",
      role: "assistant" as const,
      time: { created: 1 },
      parentID: "x",
      modelID: "m",
      providerID: "p",
      mode: "build",
      agent: "build",
      path: { cwd: "/p", root: "/p" },
      cost: 0,
      tokens: { input: 0, output: 0, reasoning: 0, cache: { read: 0, write: 0 } },
    }
    applyMessages(s, "ses", [
      { info: { ...base, error: { name: "MessageAbortedError", data: { message: "aborted" } } } as never, parts: [] },
    ])
    expect(fleetState(s, "ses")).toBe("idle")
    applyMessages(s, "ses", [
      { info: { ...base, error: { name: "ContextOverflowError", data: { message: "too long" } } } as never, parts: [] },
    ])
    expect(fleetState(s, "ses")).toBe("fault")
  })
})

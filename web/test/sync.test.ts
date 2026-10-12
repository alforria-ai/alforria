// The sync layer against a mocked API: snapshots that survive partial
// failures, transcripts that don't reload on every session update, and
// instance disposal.
import { beforeEach, describe, expect, test, vi } from "vitest"
import { createRoot, createSignal } from "solid-js"
import type { GlobalEvent, PermissionRequest, Project, Session } from "../src/api/types"

const api = vi.hoisted(() => ({}) as Record<string, ReturnType<typeof import("vitest").vi.fn>>)
vi.mock("../src/api/client", async (original) => ({
  ...(await original<typeof import("../src/api/client")>()),
  api,
}))

const { ApiError } = await import("../src/api/client")
const { onFrame, resync, retainTranscript, releaseTranscript, useTranscript } = await import("../src/sync/sync")
const { flush, setState, state } = await import("../src/store/store")
const { emptyState } = await import("../src/store/state")
const { agentsFor } = await import("../src/sync/catalog")
const { HISTORY_WINDOW } = await import("../src/fleet/fleet")

const project = (id: string, worktree: string): Project => ({
  id,
  worktree,
  vcs: id === "global" ? undefined : "git",
  time: { created: 1, updated: 1 },
  sandboxes: [],
})
const session = (id: string, directory: string, extra: Partial<Session> = {}): Session =>
  ({
    id,
    slug: id,
    projectID: "prj",
    directory,
    title: id,
    version: "1",
    time: { created: 1, updated: Date.now() },
    ...extra,
  }) as Session
const permission = (id: string, sessionID: string): PermissionRequest => ({
  id,
  sessionID,
  permission: "bash",
  patterns: ["ls"],
  metadata: {},
  always: [],
})
const frame = (type: string, properties: unknown, directory = "/a") =>
  ({ directory, payload: { id: `evt_${Math.random()}`, type, properties } }) as GlobalEvent
const notFound = () => new ApiError(404, undefined, "not found")
const settle = async () => {
  for (let i = 0; i < 5; i++) await new Promise((r) => setTimeout(r, 0))
  flush()
}

beforeEach(() => {
  setState(emptyState())
  for (const k of Object.keys(api)) delete api[k]
  Object.assign(api, {
    projects: vi.fn(async () => [project("prj", "/a")]),
    sessions: vi.fn(async () => []),
    allSessions: vi.fn(async () => ({ items: [] })),
    sessionStatus: vi.fn(async () => ({})),
    permissions: vi.fn(async () => []),
    questions: vi.fn(async () => []),
    messages: vi.fn(async () => []),
    todos: vi.fn(async () => []),
    session: vi.fn(async () => Promise.reject(notFound())),
    providers: vi.fn(async () => ({ providers: [], default: {} })),
    vcs: vi.fn(async () => ({})),
    agents: vi.fn(async () => []),
    commands: vi.fn(async () => []),
  })
})

describe("resync", () => {
  test("a transcript deleted while offline is dropped, not retried forever", async () => {
    api.sessions!.mockResolvedValue([session("ses_a", "/a")])
    await resync()
    void retainTranscript("ses_a")
    await settle()
    expect(state.loaded.ses_a).toBe(true)
    // Offline: the session is deleted; the reconnect snapshot no longer lists it.
    api.sessions!.mockResolvedValue([])
    api.messages!.mockRejectedValue(notFound())
    await expect(resync()).resolves.toBeUndefined()
    expect(state.sessions.ses_a).toBeUndefined()
    expect(state.loaded.ses_a).toBeUndefined()
    releaseTranscript("ses_a")
  })

  test("one failing directory doesn't fail the snapshot, and keeps its last known requests", async () => {
    api.sessions!.mockResolvedValue([session("ses_a", "/a"), session("ses_b", "/b")])
    api.permissions!.mockImplementation(async (dir: string) =>
      dir === "/b" ? [permission("per_b", "ses_b")] : [permission("per_a", "ses_a")],
    )
    await resync()
    expect(Object.keys(state.permissions).sort()).toEqual(["per_a", "per_b"])
    api.permissions!.mockImplementation(async (dir: string) => (dir === "/b" ? Promise.reject(new Error("boom")) : []))
    await expect(resync()).resolves.toBeUndefined()
    // /a answered (nothing pending any more); /b didn't, so its request stays.
    expect(Object.keys(state.permissions)).toEqual(["per_b"])
    expect(Object.keys(state.sessions).sort()).toEqual(["ses_a", "ses_b"])
  })

  test("asks for the history window, not the server's 100 most recent", async () => {
    await resync()
    const [dir, query] = api.sessions!.mock.calls[0]!
    expect(dir).toBe("/a")
    expect(query.scope).toBe("project")
    expect(Math.abs(query.start - (Date.now() - HISTORY_WINDOW))).toBeLessThan(5_000)
    expect(query.limit).toBeGreaterThan(100)
  })

  test("the non-git project is never asked about '/', only its sessions' folders", async () => {
    api.projects!.mockResolvedValue([project("global", "/")])
    api.allSessions!.mockImplementation(async (q: { cursor?: number }) =>
      q.cursor === undefined
        ? {
            items: [
              { ...session("ses_x", "/x", { projectID: "global" }), project: { id: "global", worktree: "/" } },
              session("ses_git", "/a"),
            ],
            next: 5,
          }
        : { items: [session("ses_y", "/y", { projectID: "global", time: { created: 1, updated: Date.now() - 1 } })] },
    )
    await resync()
    await settle()
    expect(api.sessions).not.toHaveBeenCalled()
    expect(api.allSessions).toHaveBeenCalledTimes(2)
    const asked = [api.sessionStatus, api.permissions, api.questions, api.providers].flatMap((f) =>
      f!.mock.calls.map((c) => c[0]),
    )
    expect(asked).not.toContain("/")
    expect(new Set(api.sessionStatus!.mock.calls.map((c) => c[0]))).toEqual(new Set(["/x", "/y"]))
    expect(Object.keys(state.sessions).sort()).toEqual(["ses_x", "ses_y"])
    expect(state.sessions.ses_x).not.toHaveProperty("project")
    expect(api.providers).toHaveBeenCalledWith("/x")
  })

  test("streamed text survives the activity tail", async () => {
    const info = {
      id: "msg_1",
      sessionID: "ses_a",
      role: "assistant",
      time: { created: 1 },
      tokens: { input: 0, output: 0, reasoning: 0, cache: { read: 0, write: 0 } },
      cost: 0,
    }
    const part = { id: "prt_1", sessionID: "ses_a", messageID: "msg_1", type: "text", text: "", time: { start: 1 } }
    api.sessions!.mockResolvedValue([session("ses_a", "/a")])
    api.sessionStatus!.mockResolvedValue({ ses_a: { type: "busy" } })
    let release!: () => void
    api.messages!.mockImplementation(() => new Promise((r) => (release = () => r([{ info, parts: [part] }]))))
    await resync()
    onFrame(frame("message.updated", { sessionID: "ses_a", info }))
    onFrame(frame("message.part.updated", { sessionID: "ses_a", part, time: 1 }))
    onFrame(
      frame("message.part.delta", {
        sessionID: "ses_a",
        messageID: "msg_1",
        partID: "prt_1",
        field: "text",
        delta: "Hello",
      }),
    )
    flush()
    release()
    await settle()
    expect(state.part.prt_1).toMatchObject({ text: "Hello" })
  })
})

describe("transcripts", () => {
  test("a session.updated while it is open doesn't reload it", async () => {
    setState("sessions", "ses_a", session("ses_a", "/a"))
    const [id, setId] = createSignal("ses_a")
    const dispose = createRoot((dispose) => {
      useTranscript(id, { missing: () => {}, error: () => {} })
      return dispose
    })
    await settle()
    expect(api.messages).toHaveBeenCalledTimes(1)
    onFrame(frame("session.updated", { sessionID: "ses_a", info: session("ses_a", "/a", { title: "renamed" }) }))
    await settle()
    expect(state.sessions.ses_a?.title).toBe("renamed")
    expect(api.messages).toHaveBeenCalledTimes(1)
    expect(state.loaded.ses_a).toBe(true)
    // Switching sessions does release the old one and load the new one.
    setState("sessions", "ses_b", session("ses_b", "/a"))
    setId("ses_b")
    await settle()
    expect(api.messages).toHaveBeenCalledTimes(2)
    expect(state.loaded.ses_a).toBeUndefined()
    dispose()
    expect(state.loaded.ses_b).toBeUndefined()
  })

  test("an unknown session reports missing", async () => {
    const missing = vi.fn()
    const dispose = createRoot((dispose) => {
      useTranscript(() => "ses_gone", { missing, error: () => {} })
      return dispose
    })
    await settle()
    expect(missing).toHaveBeenCalledTimes(1)
    dispose()
  })
})

describe("instance disposal", () => {
  test("re-reads that directory's requests and drops its catalogs", async () => {
    api.sessions!.mockResolvedValue([session("ses_a", "/a")])
    await resync()
    await agentsFor("/a")
    expect(api.agents).toHaveBeenCalledTimes(1)
    const before = api.permissions!.mock.calls.length
    api.permissions!.mockResolvedValue([permission("per_new", "ses_a")])
    onFrame(frame("server.instance.disposed", { directory: "/a" }))
    await settle()
    expect(api.permissions!.mock.calls.length).toBe(before + 1)
    expect(Object.keys(state.permissions)).toEqual(["per_new"])
    await agentsFor("/a")
    expect(api.agents).toHaveBeenCalledTimes(2)
  })

  test("a directory no session lives in isn't booted again", async () => {
    await resync()
    const before = api.sessionStatus!.mock.calls.length
    onFrame(frame("server.instance.disposed", { directory: "/elsewhere" }, "/elsewhere"))
    await settle()
    expect(api.sessionStatus!.mock.calls.length).toBe(before)
  })
})

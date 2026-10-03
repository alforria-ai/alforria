// Keeps the store in step with the server: snapshot on every (re)connect,
// stream in between, and per-session transcript loads on demand.
import { api, apiUrl } from "../api/client"
import type { Project } from "../api/types"
import { applyMessages, applySnapshot, type ProjectSnapshot } from "../store/reduce"
import { enqueue, setConn, setState, state, withSnapshot } from "../store/store"
import type { ModelInfo } from "../store/state"
import { connectEvents } from "./stream"

/** Directories a project's sessions live in: the worktree plus its sandboxes. */
export function projectDirs(p: Project) {
  return [p.worktree, ...(p.sandboxes ?? [])]
}

async function snapshotDir(directory: string): Promise<ProjectSnapshot> {
  const [sessions, status, permissions, questions] = await Promise.all([
    api.sessions(directory),
    api.sessionStatus(directory),
    api.permissions(directory),
    api.questions(directory),
  ])
  return { directory, sessions, status, permissions, questions }
}

export async function resync() {
  await withSnapshot(
    async () => {
      const projects = await api.projects()
      const snaps = await Promise.all(projects.flatMap(projectDirs).map(snapshotDir))
      return { projects, snaps }
    },
    (d, { projects, snaps }) => applySnapshot(d, projects, snaps),
  )
  // Re-fetch every transcript a view is showing; anything else stays lazy.
  await Promise.all(Object.keys(state.loaded).map((id) => loadTranscript(id)))
  void loadModels()
  void loadRecentActivity()
}

export async function loadTranscript(sessionID: string) {
  await withSnapshot(
    () => api.messages(sessionID),
    (d, list) => applyMessages(d, sessionID, list),
  )
}

/** Unload a transcript a view no longer shows (keeps memory flat at fleet scale). */
export function releaseTranscript(sessionID: string) {
  setState("loaded", sessionID, undefined!)
}

/**
 * The table's "now" column needs each live session's latest turn. Fetch the
 * tail of sessions that are active or waiting; idle history stays unloaded.
 */
async function loadRecentActivity() {
  const busy = new Set<string>([
    ...Object.entries(state.status)
      .filter(([, st]) => st.type !== "idle")
      .map(([id]) => id),
    ...Object.values(state.permissions).map((p) => p.sessionID),
    ...Object.values(state.questions).map((q) => q.sessionID),
  ])
  const dayAgo = Date.now() - 86_400_000
  for (const s of Object.values(state.sessions)) if (s.time.updated > dayAgo) busy.add(s.id)
  await Promise.all(
    [...busy]
      .filter((id) => state.sessions[id] && !state.loaded[id])
      .map(async (id) => {
        const [list, todos] = await Promise.all([api.messages(id, { limit: 2 }), api.todos(id).catch(() => [])])
        setState("todos", id, todos)
        for (const m of list) {
          setState("message", m.info.id, m.info)
          setState("messages", id, (ids = []) => (ids.includes(m.info.id) ? ids : [...ids, m.info.id].sort()))
          for (const p of m.parts) setState("part", p.id, p)
          setState("parts", m.info.id, m.parts.map((p) => p.id).sort())
        }
      }),
  )
}

async function loadModels() {
  const dir = state.projectOrder.map((id) => state.projects[id]?.worktree).find(Boolean)
  const list = await api.providers(dir).catch(() => undefined)
  if (!list) return
  const models: Record<string, ModelInfo> = {}
  for (const p of list.providers)
    for (const m of Object.values(p.models)) models[`${p.id}/${m.id}`] = { name: m.name, context: m.limit?.context }
  setState("models", models)
}

/**
 * A snapshot that fails (server restarting, network blip) must not leave the
 * view on stale state while the stream looks live: retry with backoff until
 * one lands. A newer call supersedes older retries.
 */
let generation = 0
async function resyncWithRetry() {
  const mine = ++generation
  for (let attempt = 0; mine === generation; attempt++) {
    try {
      await resync()
      return
    } catch (err) {
      console.warn(`resync failed (attempt ${attempt + 1})`, err)
      await new Promise((r) => setTimeout(r, Math.min(15_000, 500 * 2 ** attempt)))
    }
  }
}

let stop: (() => void) | undefined
export function startSync() {
  stop?.()
  stop = connectEvents(apiUrl("/global/event"), {
    onFrame: (frame) => {
      enqueue(frame)
      // A global config change disposes every instance server-side; re-read everything.
      if (frame.payload.type === "global.disposed") void resyncWithRetry()
    },
    onOpen: () => void resyncWithRetry(),
    onState: (s, attempt, retryAt) => setConn({ state: s, attempt, retryAt }),
  })
  return stop
}

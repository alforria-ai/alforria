// Keeps the store in step with the server: snapshot on every (re)connect,
// stream in between, and per-session transcript loads on demand.
import { createEffect, on, onCleanup } from "solid-js"
import { produce } from "solid-js/store"
import { api, ApiError, apiUrl } from "../api/client"
import type { Project, Session } from "../api/types"
import { anyDirectory, GLOBAL_PROJECT, HISTORY_WINDOW } from "../fleet/fleet"
import {
  applyDirSnapshot,
  applyMessages,
  applySnapshot,
  applyTail,
  dropSession,
  trimToTail,
  UNREAD_TAIL,
  type ProjectSnapshot,
} from "../store/reduce"
import { enqueue, setConn, setState, state, withSnapshot } from "../store/store"
import type { ModelInfo } from "../store/state"
import { dropCatalog } from "./catalog"
import { connectEvents } from "./stream"

/** Directories a project's sessions live in: the worktree plus its sandboxes. */
export function projectDirs(p: Project) {
  return [p.worktree, ...(p.sandboxes ?? [])]
}

/** Sessions per project in one snapshot; the server's default would be 100. */
const SESSION_CAP = 1000

const notFound = (err: unknown) => err instanceof ApiError && err.status === 404

/**
 * One directory's status and pending requests. A directory that fails to
 * answer is marked, not fatal: the rest of the fleet still lands.
 */
async function snapshotDir(directory: string, sessions: Session[]): Promise<ProjectSnapshot> {
  const [status, permissions, questions] = await Promise.allSettled([
    api.sessionStatus(directory),
    api.permissions(directory),
    api.questions(directory),
  ])
  if (status.status === "fulfilled" && permissions.status === "fulfilled" && questions.status === "fulfilled")
    return { directory, sessions, status: status.value, permissions: permissions.value, questions: questions.value }
  console.warn(`snapshot of ${directory} failed`)
  return { directory, sessions, status: {}, permissions: [], questions: [], failed: true }
}

/**
 * The non-git "global" project's sessions. Its worktree is "/", and asking
 * there would boot an instance at the root: list server-wide instead, a page
 * at a time, and keep its own.
 */
async function globalSessions(start: number) {
  const out: Session[] = []
  for (let cursor: number | undefined; ;) {
    const page = await api.allSessions({ start, cursor, limit: 200 })
    for (const { project: _, ...s } of page.items) if (s.projectID === GLOBAL_PROJECT) out.push(s)
    if (page.next === undefined) return out
    cursor = page.next
  }
}

/**
 * A project's sessions can live outside its worktree: in a subfolder of the
 * repo, or anywhere at all for the non-git "global" project. List those
 * updated within the history window project-wide, then read status and
 * pending requests per directory in use.
 */
async function snapshotProject(p: Project, start: number): Promise<ProjectSnapshot[]> {
  const global = p.id === GLOBAL_PROJECT
  const sessions = global
    ? await globalSessions(start)
    : await api.sessions(p.worktree, { scope: "project", start, limit: SESSION_CAP })
  const dirs = new Set(global ? [] : projectDirs(p))
  for (const s of sessions) dirs.add(s.directory)
  return Promise.all([...dirs].map((d, i) => snapshotDir(d, i === 0 ? sessions : [])))
}

export async function resync() {
  const start = Date.now() - HISTORY_WINDOW
  await withSnapshot(
    async () => {
      const projects = await api.projects()
      const snaps = (await Promise.all(projects.map((p) => snapshotProject(p, start)))).flat()
      return { projects, snaps }
    },
    (d, { projects, snaps }) => applySnapshot(d, projects, snaps, [...viewers.keys(), ...Object.keys(d.loaded)]),
  )
  void loadLiveRows()
  // Re-fetch every transcript a view is showing, each on its own: one that
  // fails must not fail the snapshot (a 404 is already handled inside).
  await Promise.all(
    [...viewers.keys()].map((id) => loadTranscript(id).catch((err) => console.warn(`transcript ${id}`, err))),
  )
  void loadModels()
  void loadRecentActivity()
  void loadBranches()
}

/** How many views show each transcript; at zero it is unloaded. */
const viewers = new Map<string, number>()

/** A session the server no longer has: drop it, and views showing it close. */
function forget(sessionID: string) {
  setState(produce((d) => dropSession(d, sessionID)))
}

export async function loadTranscript(sessionID: string) {
  try {
    await withSnapshot(
      () => api.messages(sessionID),
      (d, list) => applyMessages(d, sessionID, list),
      // A view may close while the fetch is in flight: don't resurrect it.
      () => (viewers.get(sessionID) ?? 0) > 0,
    )
  } catch (err) {
    if (!notFound(err)) throw err
    forget(sessionID)
  }
}

/** A session's row, fetched on its own (one older than the window). False if it doesn't exist. */
async function loadSession(sessionID: string) {
  try {
    await withSnapshot(
      () => api.session(sessionID),
      (d, s) => void (d.sessions[s.id] ??= s),
    )
    return true
  } catch (err) {
    return !notFound(err)
  }
}

/** A view starts showing a transcript: load it and keep it live. */
export function retainTranscript(sessionID: string) {
  viewers.set(sessionID, (viewers.get(sessionID) ?? 0) + 1)
  return loadTranscript(sessionID)
}

/** A view stops showing it: at zero viewers, drop all but the latest turn. */
export function releaseTranscript(sessionID: string) {
  const n = (viewers.get(sessionID) ?? 1) - 1
  if (n > 0) return void viewers.set(sessionID, n)
  viewers.delete(sessionID)
  setState(
    produce((d) => {
      delete d.loaded[sessionID]
      trimToTail(d, sessionID)
    }),
  )
}

/**
 * Keep `id()`'s transcript loaded and live while the calling view is mounted.
 * Keyed on the id alone: a `session.updated` (every send) must not release
 * and re-fetch it, which flashes "Loading…" and loses scroll and drafts.
 * `missing` fires when the session doesn't exist (an old link).
 */
export function useTranscript(id: () => string, handlers: { missing(): void; error(err: Error): void }) {
  createEffect(
    on(id, (sid) => {
      if (!state.sessions[sid]) void loadSession(sid).then((found) => found || handlers.missing())
      void retainTranscript(sid).catch(handlers.error)
      onCleanup(() => releaseTranscript(sid))
    }),
  )
}

/** Sessions that are busy or waiting keep a row even when older than the window. */
async function loadLiveRows() {
  const ids = new Set([
    ...Object.entries(state.status)
      .filter(([, st]) => st.type !== "idle")
      .map(([id]) => id),
    ...Object.values(state.permissions).map((p) => p.sessionID),
    ...Object.values(state.questions).map((q) => q.sessionID),
  ])
  await Promise.all([...ids].filter((id) => !state.sessions[id]).map(loadSession))
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
  const unread = (id: string) => !!state.sessions[id] && !state.loaded[id]
  await Promise.all(
    [...busy].filter(unread).map((id) =>
      withSnapshot(
        () => Promise.all([api.messages(id, { limit: UNREAD_TAIL }), api.todos(id).catch(() => undefined)]),
        (d, [list, todos]) => {
          if (todos) d.todos[id] = todos
          applyTail(d, id, list)
        },
        // A view may have opened it meanwhile: its full transcript wins.
        () => unread(id),
      ).catch(() => undefined),
    ),
  )
}

async function loadBranches() {
  await Promise.all(
    state.projectOrder.map(async (id) => {
      const p = state.projects[id]
      if (!p || p.vcs !== "git") return
      const info = await api.vcs(p.worktree).catch(() => undefined)
      if (info?.branch) setState("branches", id, info.branch)
    }),
  )
}

/** The model list behind the composer's picker; reload after credentials change. */
export async function loadModels() {
  const list = await api.providers(anyDirectory()).catch(() => undefined)
  if (!list) return
  const models: Record<string, ModelInfo> = {}
  for (const p of list.providers)
    for (const m of Object.values(p.models)) models[`${p.id}/${m.id}`] = { name: m.name, context: m.limit?.context }
  setState("models", models)
}

/**
 * An instance was disposed (its config changed, or it was reloaded): what it
 * served may have changed. Re-read its status and requests, and its catalogs.
 * A directory no session lives in is left alone; asking would boot it again.
 */
async function resnapshotDir(directory: string) {
  dropCatalog(directory)
  if (!Object.values(state.sessions).some((s) => s.directory === directory)) return
  await withSnapshot(
    () => snapshotDir(directory, []),
    (d, snap) => {
      if (!snap.failed) applyDirSnapshot(d, snap)
    },
  )
  if (directory === anyDirectory()) void loadModels()
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

/** Side effects of a frame beyond the reducer: re-reads the store can't do itself. */
export function onFrame(frame: Parameters<typeof enqueue>[0]) {
  enqueue(frame)
  const ev = frame.payload
  // A global config change disposes every instance server-side; re-read everything.
  if (ev.type === "global.disposed") {
    dropCatalog()
    void resyncWithRetry()
  } else if (ev.type === "server.instance.disposed") void resnapshotDir(ev.properties.directory)
}

let stop: (() => void) | undefined
export function startSync() {
  stop?.()
  stop = connectEvents(apiUrl("/global/event"), {
    onFrame,
    onOpen: () => void resyncWithRetry(),
    onState: (s, attempt, retryAt) => setConn({ state: s, attempt, retryAt }),
  })
  return stop
}

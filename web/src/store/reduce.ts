// Pure reducers over a mutable draft: the Solid store applies them inside
// `produce`, tests apply them to plain objects. Nothing here touches the DOM
// or the network.
import type {
  GlobalEvent,
  MessageWithParts,
  Part,
  Payload,
  PermissionRequest,
  Project,
  QuestionRequest,
  Session,
  SessionStatus,
} from "../api/types"
import { insertSorted, removeId, type State } from "./state"

/** Apply one `/global/event` frame. Unknown and `sync` frames are ignored. */
export function reduce(s: State, frame: GlobalEvent) {
  const ev = frame.payload
  s.conn.lastFrame = Date.now()
  switch (ev.type) {
    case "session.created":
    case "session.updated":
      s.sessions[ev.properties.info.id] = ev.properties.info
      return
    case "session.deleted":
      dropSession(s, ev.properties.info.id)
      return
    case "session.status":
      s.status[ev.properties.sessionID] = ev.properties.status
      return
    case "session.idle":
      s.status[ev.properties.sessionID] = { type: "idle" }
      return
    case "session.error": {
      const { sessionID, error } = ev.properties as {
        sessionID?: string
        error?: { name: string; data?: { message?: string } }
      }
      if (sessionID && error) s.errors[sessionID] = { name: error.name, message: error.data?.message ?? "" }
      return
    }
    case "session.diff":
      s.diffs[ev.properties.sessionID] = ev.properties.diff as State["diffs"][string]
      return
    case "message.updated": {
      const info = ev.properties.info
      s.message[info.id] = info
      const ids = (s.messages[info.sessionID] ??= [])
      insertSorted(ids, info.id)
      if (info.role === "user") delete s.errors[info.sessionID]
      // Sessions nobody is reading keep only their latest turn (for the
      // table's "now" column); memory stays flat at fleet scale.
      if (!s.loaded[info.sessionID]) trimToTail(s, info.sessionID)
      return
    }
    case "message.removed": {
      const { sessionID, messageID } = ev.properties
      dropMessage(s, sessionID, messageID)
      return
    }
    case "message.part.updated": {
      const part = ev.properties.part
      // A part for a message we trimmed (or never saw) of an unread session.
      if (!s.loaded[part.sessionID] && !s.message[part.messageID]) return
      upsertPart(s, part)
      return
    }
    case "message.part.delta": {
      const { partID, field, delta } = ev.properties
      const part = s.part[partID] as Record<string, unknown> | undefined
      if (part) part[field] = `${(part[field] as string | undefined) ?? ""}${delta}`
      return
    }
    case "message.part.removed": {
      const { messageID, partID } = ev.properties
      delete s.part[partID]
      removeId(s.parts[messageID], partID)
      return
    }
    case "permission.asked":
      s.permissions[ev.properties.id] = ev.properties
      return
    case "permission.replied":
      delete s.permissions[ev.properties.requestID]
      return
    case "question.asked":
      s.questions[ev.properties.id] = ev.properties as QuestionRequest
      return
    case "question.replied":
    case "question.rejected":
      delete s.questions[(ev as Payload<"question.replied">).properties.requestID]
      return
    case "todo.updated":
      s.todos[ev.properties.sessionID] = ev.properties.todos
      return
    case "vcs.branch.updated": {
      const branch = (ev.properties as { branch?: string }).branch
      const project = Object.values(s.projects).find((p) => p.worktree === frame.directory)
      if (project && branch) s.branches[project.id] = branch
      return
    }
    case "project.updated": {
      const project = ev.properties as Project
      if (!s.projects[project.id]) s.projectOrder.push(project.id)
      s.projects[project.id] = project
      return
    }
  }
}

function upsertPart(s: State, part: Part) {
  s.part[part.id] = part
  insertSorted((s.parts[part.messageID] ??= []), part.id)
}

function dropMessage(s: State, sessionID: string, messageID: string) {
  for (const pid of s.parts[messageID] ?? []) delete s.part[pid]
  delete s.parts[messageID]
  delete s.message[messageID]
  removeId(s.messages[sessionID], messageID)
}

/** Messages kept for sessions without an open transcript. */
export const UNREAD_TAIL = 2

export function trimToTail(s: State, sessionID: string, keep = UNREAD_TAIL) {
  const ids = s.messages[sessionID]
  if (!ids || ids.length <= keep) return
  for (const mid of ids.slice(0, ids.length - keep)) dropMessage(s, sessionID, mid)
}

/** Forget a deleted session and everything that hangs off it. */
export function dropSession(s: State, sessionID: string) {
  dropSessionData(s, sessionID)
  delete s.sessions[sessionID]
  delete s.status[sessionID]
  for (const [id, p] of Object.entries(s.permissions)) if (p.sessionID === sessionID) delete s.permissions[id]
  for (const [id, q] of Object.entries(s.questions)) if (q.sessionID === sessionID) delete s.questions[id]
}

/** A session's transcript and per-session records; its row stays. */
function dropSessionData(s: State, sessionID: string) {
  for (const mid of [...(s.messages[sessionID] ?? [])]) dropMessage(s, sessionID, mid)
  delete s.messages[sessionID]
  delete s.todos[sessionID]
  delete s.diffs[sessionID]
  delete s.errors[sessionID]
  delete s.loaded[sessionID]
}

/** Per-directory REST snapshot taken on (re)connect. */
export interface ProjectSnapshot {
  directory: string
  sessions: Session[]
  status: Record<string, SessionStatus>
  permissions: PermissionRequest[]
  questions: QuestionRequest[]
  /** Its status and requests could not be read: keep what the store already has for it. */
  failed?: boolean
}

/**
 * Replace fleet-level state with a fresh snapshot. Sessions, status and the
 * waiting-on-you records are authoritative from REST; anything the snapshot
 * no longer lists is gone (e.g. a permission answered while we were offline).
 * `keep` lists sessions a view shows: they stay even when the snapshot (the
 * history window) no longer covers them.
 */
export function applySnapshot(
  s: State,
  projects: Project[],
  snaps: ProjectSnapshot[],
  keep: Iterable<string> = Object.keys(s.loaded),
) {
  s.projects = Object.fromEntries(projects.map((p) => [p.id, p]))
  s.projectOrder = projects.map((p) => p.id)
  const sessions: State["sessions"] = {}
  const status: State["status"] = {}
  const permissions: State["permissions"] = {}
  const questions: State["questions"] = {}
  const kept = new Set(keep)
  for (const snap of snaps) for (const session of snap.sessions) sessions[session.id] = session
  for (const id of kept) if (!sessions[id] && s.sessions[id]) sessions[id] = s.sessions[id]
  // A directory that failed to answer keeps its last known state rather than
  // reading as "nothing pending".
  const stale = new Set(snaps.filter((x) => x.failed).map((x) => x.directory))
  const inStale = (sid: string) => stale.has((sessions[sid] ?? s.sessions[sid])?.directory ?? "")
  if (stale.size) {
    for (const [id, st] of Object.entries(s.status)) if (inStale(id)) status[id] = st
    for (const p of Object.values(s.permissions)) if (inStale(p.sessionID)) permissions[p.id] = p
    for (const q of Object.values(s.questions)) if (inStale(q.sessionID)) questions[q.id] = q
  }
  for (const snap of snaps) {
    if (snap.failed) continue
    Object.assign(status, snap.status)
    for (const p of snap.permissions) permissions[p.id] = p
    for (const q of snap.questions) questions[q.id] = q
  }
  // A session that is busy or waiting keeps its row however old it is.
  const live = [
    ...Object.keys(status),
    ...Object.values(permissions).map((p) => p.sessionID),
    ...Object.values(questions).map((q) => q.sessionID),
  ]
  for (const id of live) if (!sessions[id] && s.sessions[id]) sessions[id] = s.sessions[id]
  // Sessions that left the snapshot take their transcripts and records along.
  const held = [s.sessions, s.messages, s.todos, s.diffs, s.errors, s.loaded].flatMap((m) => Object.keys(m))
  for (const id of new Set(held)) if (!sessions[id] && !kept.has(id)) dropSessionData(s, id)
  s.sessions = sessions
  s.status = status
  s.permissions = permissions
  s.questions = questions
  s.synced = true
}

/**
 * Replace one directory's status and pending requests, e.g. after its
 * instance was disposed and booted again.
 */
export function applyDirSnapshot(s: State, snap: ProjectSnapshot) {
  const here = (sid: string) => s.sessions[sid]?.directory === snap.directory
  for (const id of Object.keys(s.status)) if (here(id)) delete s.status[id]
  for (const [id, p] of Object.entries(s.permissions)) if (here(p.sessionID)) delete s.permissions[id]
  for (const [id, q] of Object.entries(s.questions)) if (here(q.sessionID)) delete s.questions[id]
  Object.assign(s.status, snap.status)
  for (const p of snap.permissions) s.permissions[p.id] = p
  for (const q of snap.questions) s.questions[q.id] = q
}

/**
 * The server returns unfinished text parts with empty text (only deltas carry
 * progress). Keep what has already streamed in rather than wiping it.
 */
function keepStreamed(prev: Part | undefined, part: Part): Part {
  if (part.type !== "text" && part.type !== "reasoning") return part
  if (part.time?.end || prev?.type !== part.type) return part
  return prev.text.length > part.text.length ? { ...part, text: prev.text } : part
}

/** Replace one session's transcript with `GET /session/{id}/message`. */
export function applyMessages(s: State, sessionID: string, list: MessageWithParts[]) {
  const prev = new Map<string, Part>()
  for (const mid of s.messages[sessionID] ?? [])
    for (const pid of s.parts[mid] ?? []) {
      const p = s.part[pid]
      if (p) prev.set(pid, p)
    }
  for (const mid of [...(s.messages[sessionID] ?? [])]) dropMessage(s, sessionID, mid)
  const ids: string[] = []
  for (const { info, parts } of list) {
    s.message[info.id] = info
    insertSorted(ids, info.id)
    const pids: string[] = []
    for (const part of parts) {
      s.part[part.id] = keepStreamed(prev.get(part.id), part)
      insertSorted(pids, part.id)
    }
    s.parts[info.id] = pids
  }
  s.messages[sessionID] = ids
  s.loaded[sessionID] = true
}

/**
 * Merge the latest turns of a session nobody is reading (the table's "now"
 * column). An open transcript is already complete and live: leave it be.
 */
export function applyTail(s: State, sessionID: string, list: MessageWithParts[]) {
  if (s.loaded[sessionID] || !s.sessions[sessionID]) return
  const ids = (s.messages[sessionID] ??= [])
  for (const { info, parts } of list) {
    s.message[info.id] = info
    insertSorted(ids, info.id)
    const pids = (s.parts[info.id] ??= [])
    for (const part of parts) {
      s.part[part.id] = keepStreamed(s.part[part.id], part)
      insertSorted(pids, part.id)
    }
  }
  trimToTail(s, sessionID)
}

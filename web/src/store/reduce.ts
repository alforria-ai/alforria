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

function dropSession(s: State, sessionID: string) {
  for (const mid of [...(s.messages[sessionID] ?? [])]) dropMessage(s, sessionID, mid)
  delete s.messages[sessionID]
  delete s.sessions[sessionID]
  delete s.status[sessionID]
  delete s.todos[sessionID]
  delete s.diffs[sessionID]
  delete s.errors[sessionID]
  delete s.loaded[sessionID]
  for (const [id, p] of Object.entries(s.permissions)) if (p.sessionID === sessionID) delete s.permissions[id]
  for (const [id, q] of Object.entries(s.questions)) if (q.sessionID === sessionID) delete s.questions[id]
}

/** Per-project REST snapshot taken on (re)connect. */
export interface ProjectSnapshot {
  directory: string
  sessions: Session[]
  status: Record<string, SessionStatus>
  permissions: PermissionRequest[]
  questions: QuestionRequest[]
}

/**
 * Replace fleet-level state with a fresh snapshot. Sessions, status and the
 * waiting-on-you records are authoritative from REST; anything the snapshot
 * no longer lists is gone (e.g. a permission answered while we were offline).
 */
export function applySnapshot(s: State, projects: Project[], snaps: ProjectSnapshot[]) {
  s.projects = Object.fromEntries(projects.map((p) => [p.id, p]))
  s.projectOrder = projects.map((p) => p.id)
  const sessions: State["sessions"] = {}
  const status: State["status"] = {}
  const permissions: State["permissions"] = {}
  const questions: State["questions"] = {}
  for (const snap of snaps) {
    for (const session of snap.sessions) sessions[session.id] = session
    Object.assign(status, snap.status)
    for (const p of snap.permissions) permissions[p.id] = p
    for (const q of snap.questions) questions[q.id] = q
  }
  // The snapshot lists the 100 most recent sessions per directory. Sessions
  // a view has open (fetched on demand, maybe older) are kept, not dropped.
  for (const id of Object.keys(s.loaded)) if (!sessions[id] && s.sessions[id]) sessions[id] = s.sessions[id]
  s.sessions = sessions
  s.status = status
  s.permissions = permissions
  s.questions = questions
}

/** Replace one session's transcript with `GET /session/{id}/message`. */
export function applyMessages(s: State, sessionID: string, list: MessageWithParts[]) {
  // The server returns unfinished text parts with empty text (only deltas
  // carry progress). Keep what has already streamed in rather than wiping it.
  const streamed = new Map<string, string>()
  for (const mid of s.messages[sessionID] ?? [])
    for (const pid of s.parts[mid] ?? []) {
      const p = s.part[pid]
      if (p && (p.type === "text" || p.type === "reasoning")) streamed.set(pid, p.text)
    }
  for (const mid of [...(s.messages[sessionID] ?? [])]) dropMessage(s, sessionID, mid)
  const ids: string[] = []
  for (const { info, parts } of list) {
    s.message[info.id] = info
    insertSorted(ids, info.id)
    const pids: string[] = []
    for (const part of parts) {
      const prev = streamed.get(part.id)
      const open = (part.type === "text" || part.type === "reasoning") && !part.time?.end
      s.part[part.id] = open && prev && prev.length > part.text.length ? { ...part, text: prev } : part
      insertSorted(pids, part.id)
    }
    s.parts[info.id] = pids
  }
  s.messages[sessionID] = ids
  s.loaded[sessionID] = true
}

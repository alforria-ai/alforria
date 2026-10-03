// The normalized client state. One copy of every record, keyed by id: a
// permission or question exists once however many views render it, and a
// streamed delta touches exactly one part.
import type {
  FileDiff,
  Message,
  Part,
  PermissionRequest,
  Project,
  QuestionRequest,
  Session,
  SessionStatus,
  Todo,
} from "../api/types"

export type ConnState = "connecting" | "live" | "retrying"

export interface SessionError {
  name: string
  message: string
}

export interface ModelInfo {
  name: string
  context?: number
}

export interface State {
  projects: Record<string, Project>
  /** Project ids in display order. */
  projectOrder: string[]
  sessions: Record<string, Session>
  status: Record<string, SessionStatus>
  /** sessionID → message ids, ascending (ids sort by creation time). */
  messages: Record<string, string[]>
  message: Record<string, Message>
  /** messageID → part ids, ascending. */
  parts: Record<string, string[]>
  part: Record<string, Part>
  permissions: Record<string, PermissionRequest>
  questions: Record<string, QuestionRequest>
  todos: Record<string, Todo[]>
  diffs: Record<string, FileDiff[]>
  errors: Record<string, SessionError>
  /** Sessions whose transcript has been fetched and is kept live. */
  loaded: Record<string, boolean>
  /** projectID → current VCS branch (from GET /vcs, kept live by vcs.branch.updated). */
  branches: Record<string, string>
  /** `providerID/modelID` → display info (context window for the ctx meter). */
  models: Record<string, ModelInfo>
  conn: { state: ConnState; lastFrame: number; attempt: number; retryAt: number }
}

export function emptyState(): State {
  return {
    projects: {},
    projectOrder: [],
    sessions: {},
    status: {},
    messages: {},
    message: {},
    parts: {},
    part: {},
    permissions: {},
    questions: {},
    todos: {},
    diffs: {},
    errors: {},
    loaded: {},
    branches: {},
    models: {},
    conn: { state: "connecting", lastFrame: 0, attempt: 0, retryAt: 0 },
  }
}

/** Insert `id` into an ascending id list, keeping it sorted and unique. */
export function insertSorted(list: string[], id: string) {
  let lo = 0
  let hi = list.length
  while (lo < hi) {
    const mid = (lo + hi) >> 1
    const v = list[mid]!
    if (v === id) return
    if (v < id) lo = mid + 1
    else hi = mid
  }
  list.splice(lo, 0, id)
}

export function removeId(list: string[] | undefined, id: string) {
  if (!list) return
  const i = list.indexOf(id)
  if (i >= 0) list.splice(i, 1)
}

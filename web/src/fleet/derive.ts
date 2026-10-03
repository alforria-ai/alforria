// Per-session aggregates for the fleet views (band counter, queue, table,
// session list). Everything is derived from the store; nothing is cached here,
// so callers wrap these in Solid memos where they need stability.
import type {
  AssistantMessage,
  Part,
  PermissionRequest,
  QuestionRequest,
  SnapshotFileDiff,
  ToolPart,
  UserMessage,
} from "../api/types"
import type { State } from "../store/state"

export type FleetState = "waiting" | "working" | "retry" | "fault" | "idle"

export interface Activity {
  /** Tool name, or "writing" / "thinking" / "permission" / "question" / "retry" / "fault" / "done". */
  kind: string
  text: string
  /** True when `text` is prose rather than a command, path or identifier. */
  prose: boolean
}

/** A pending permission or question: one entry of the interrupt queue. */
export type Interrupt =
  | { kind: "permission"; id: string; sessionID: string; request: PermissionRequest; since: number }
  | { kind: "question"; id: string; sessionID: string; request: QuestionRequest; since: number }

export function interruptsFor(s: State, sessionID: string): Interrupt[] {
  return allInterrupts(s).filter((i) => i.sessionID === sessionID)
}

/**
 * Every pending permission and question, oldest first. Request ids are
 * time-ordered (`per_…`/`que_…` ULIDs), so sorting by id is sorting by age;
 * `since` is best-effort from the session's last update.
 */
export function allInterrupts(s: State): Interrupt[] {
  const out: Interrupt[] = []
  for (const request of Object.values(s.permissions))
    out.push({
      kind: "permission",
      id: request.id,
      sessionID: request.sessionID,
      request,
      since: askedAt(s, request.sessionID, request.tool?.messageID),
    })
  for (const request of Object.values(s.questions))
    out.push({
      kind: "question",
      id: request.id,
      sessionID: request.sessionID,
      request,
      since: askedAt(s, request.sessionID, request.tool?.messageID),
    })
  return out.sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0))
}

function askedAt(s: State, sessionID: string, messageID?: string) {
  const msg = messageID ? s.message[messageID] : undefined
  if (msg) return msg.time.created
  return s.sessions[sessionID]?.time.updated ?? Date.now()
}

/**
 * The session's turns that changed files, newest first: each user message
 * carries its turn's diff in `summary.diffs` (first step-start snapshot to
 * last step-finish snapshot), as in TS.
 */
export function turnDiffs(s: State, sessionID: string) {
  const out: { message: UserMessage; diffs: SnapshotFileDiff[] }[] = []
  const ids = s.messages[sessionID] ?? []
  for (let i = ids.length - 1; i >= 0; i--) {
    const m = s.message[ids[i]!]
    if (m?.role !== "user") continue
    const diffs = (m as UserMessage).summary?.diffs ?? []
    if (diffs.length) out.push({ message: m as UserMessage, diffs })
  }
  return out
}

export function lastAssistant(s: State, sessionID: string): AssistantMessage | undefined {
  const ids = s.messages[sessionID]
  if (!ids) return
  for (let i = ids.length - 1; i >= 0; i--) {
    const m = s.message[ids[i]!]
    if (m?.role === "assistant") return m
  }
}

function lastPart(s: State, messageID: string): Part | undefined {
  const ids = s.parts[messageID]
  if (!ids) return
  for (let i = ids.length - 1; i >= 0; i--) {
    const p = s.part[ids[i]!]
    if (p && p.type !== "step-start" && p.type !== "step-finish" && p.type !== "snapshot" && p.type !== "patch")
      return p
  }
}

export function fleetState(s: State, sessionID: string): FleetState {
  if (Object.values(s.permissions).some((p) => p.sessionID === sessionID)) return "waiting"
  if (Object.values(s.questions).some((q) => q.sessionID === sessionID)) return "waiting"
  const st = s.status[sessionID]
  if (st?.type === "busy") return "working"
  if (st?.type === "retry") return "retry"
  if (s.errors[sessionID]) return "fault"
  const last = lastAssistant(s, sessionID)
  if (last?.error && last.error.name !== "MessageAbortedError") return "fault"
  return "idle"
}

export function activity(s: State, sessionID: string): Activity {
  const state = fleetState(s, sessionID)
  if (state === "waiting") {
    const i = interruptsFor(s, sessionID)[0]!
    if (i.kind === "question") return { kind: "question", text: i.request.questions[0]?.question ?? "", prose: true }
    return { kind: "permission", text: permissionSummary(i.request), prose: false }
  }
  if (state === "retry") {
    const st = s.status[sessionID] as { attempt: number; message: string; next: number }
    const wait = Math.max(0, Math.round((st.next - Date.now()) / 1000))
    return { kind: "retry", text: `${st.message} · attempt ${st.attempt}${wait ? ` in ${wait}s` : ""}`, prose: true }
  }
  if (state === "fault") {
    const err = s.errors[sessionID] ?? errorOf(lastAssistant(s, sessionID))
    return { kind: "fault", text: [err?.name, err?.message].filter(Boolean).join(" · "), prose: false }
  }
  const last = lastAssistant(s, sessionID)
  const part = last && lastPart(s, last.id)
  if (state === "working") {
    if (!part) return { kind: "thinking", text: "", prose: true }
    if (part.type === "tool") return { kind: part.tool, text: toolTarget(part), prose: false }
    if (part.type === "reasoning") return { kind: "thinking", text: firstLine(part.text), prose: true }
    if (part.type === "text") return { kind: "writing", text: firstLine(part.text), prose: true }
    return { kind: "working", text: "", prose: true }
  }
  if (part?.type === "text") return { kind: "done", text: firstLine(part.text), prose: true }
  const changed = turnDiffs(s, sessionID)[0]?.diffs.length
  if (changed) return { kind: "done", text: `${changed} ${changed === 1 ? "file" : "files"} changed`, prose: true }
  return { kind: "done", text: "", prose: true }
}

function errorOf(m?: AssistantMessage) {
  if (!m?.error) return undefined
  const data = (m.error as { data?: { message?: string } }).data
  return { name: m.error.name, message: data?.message ?? "" }
}

export function permissionSummary(p: PermissionRequest) {
  const patterns = p.patterns.filter((x) => x && x !== "*")
  return [p.permission, ...patterns].join("  ")
}

function firstLine(text: string) {
  const line =
    text
      .trim()
      .split("\n")
      .find((l) => l.trim()) ?? ""
  return line.length > 200 ? `${line.slice(0, 200)}…` : line
}

/** The one-line target of a tool call: the command, path, pattern or URL. */
export function toolTarget(part: ToolPart): string {
  const state = part.state as { input?: Record<string, unknown>; title?: string }
  const input = state.input ?? {}
  const str = (k: string) => (typeof input[k] === "string" ? (input[k] as string) : "")
  switch (part.tool) {
    case "bash":
      return str("command") || str("description")
    case "read":
    case "edit":
    case "write":
      return str("filePath")
    case "grep":
      return [str("pattern"), str("path")].filter(Boolean).join("  ")
    case "glob":
      return [str("pattern"), str("path")].filter(Boolean).join("  ")
    case "webfetch":
      return str("url")
    case "websearch":
      return str("query")
    case "task":
      return str("description")
    case "todowrite":
      return Array.isArray(input.todos) ? `${input.todos.length} todos` : ""
    case "skill":
      return str("name")
  }
  return state.title ?? ""
}

export function todoProgress(s: State, sessionID: string): [done: number, total: number] | null {
  const todos = s.todos[sessionID]
  if (!todos?.length) return null
  const live = todos.filter((t) => t.status !== "cancelled")
  return [live.filter((t) => t.status === "completed").length, live.length]
}

/** Context use of the latest assistant turn, as a fraction of the model window. */
export function contextUse(s: State, sessionID: string): { tokens: number; fraction: number | null } | null {
  const m = lastAssistant(s, sessionID)
  if (!m) return null
  const t = m.tokens
  const tokens = t.input + t.output + t.reasoning + t.cache.read + t.cache.write
  if (!tokens) return null
  const window = s.models[`${m.providerID}/${m.modelID}`]?.context
  return { tokens, fraction: window ? tokens / window : null }
}

export function sessionCost(s: State, sessionID: string): number {
  const session = s.sessions[sessionID]
  if (session?.cost != null) return session.cost
  let total = 0
  for (const id of s.messages[sessionID] ?? []) {
    const m = s.message[id]
    if (m?.role === "assistant") total += m.cost
  }
  return total
}

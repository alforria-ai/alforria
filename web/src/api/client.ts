// Typed client for the v1 HTTP API. Project-scoped calls pass `?directory=`
// (the server's location middleware reads it before the header); session
// routes resolve their directory from the session row. Same-origin requests
// carry the browser's Basic-auth credentials when the server has a password.
import type {
  Agent,
  Command,
  SnapshotFileDiff,
  VcsFileDiff,
  MessageWithParts,
  PermissionReply,
  Project,
  PermissionRequest,
  QuestionRequest,
  Session,
  SessionStatus,
  Todo,
} from "./types"

export class ApiError extends Error {
  constructor(
    readonly status: number,
    readonly body: unknown,
    message: string,
  ) {
    super(message)
    this.name = "ApiError"
  }
}

type Query = Record<string, string | number | boolean | undefined>

interface RequestOptions {
  directory?: string
  query?: Query
  body?: unknown
  signal?: AbortSignal
}

/** Base URL for API calls; empty means same origin (the embedded case). */
let base = ""
export function setBaseUrl(url: string) {
  base = url.replace(/\/$/, "")
}
export function apiUrl(path: string, query: Query = {}) {
  const params = new URLSearchParams()
  for (const [k, v] of Object.entries(query)) if (v !== undefined) params.set(k, String(v))
  const qs = params.toString()
  return `${base}${path}${qs ? `?${qs}` : ""}`
}

async function request<T>(method: string, path: string, opts: RequestOptions = {}): Promise<T> {
  const query = { ...opts.query, directory: opts.directory }
  const res = await fetch(apiUrl(path, query), {
    method,
    signal: opts.signal,
    credentials: "same-origin",
    headers: opts.body === undefined ? undefined : { "content-type": "application/json" },
    body: opts.body === undefined ? undefined : JSON.stringify(opts.body),
  })
  if (res.status === 204) return undefined as T
  const text = await res.text()
  const body = text ? safeJson(text) : undefined
  if (!res.ok) {
    const detail = typeof body === "object" && body && "message" in body ? String(body.message) : text.slice(0, 200)
    throw new ApiError(res.status, body, `${method} ${path} → ${res.status}${detail ? `: ${detail}` : ""}`)
  }
  return body as T
}

function safeJson(text: string): unknown {
  try {
    return JSON.parse(text)
  } catch {
    return text
  }
}

export interface AuthMethod {
  type: "oauth" | "api"
  label: string
}

export interface OAuthAuthorization {
  url: string
  method: "auto" | "code"
  instructions: string
}

export interface PromptPart {
  type: "text" | "file" | "agent"
  text?: string
  url?: string
  mime?: string
  filename?: string
  name?: string
}

export interface PromptInput {
  parts: PromptPart[]
  agent?: string
  model?: { providerID: string; modelID: string }
  messageID?: string
}

export interface ProviderList {
  providers: {
    id: string
    name: string
    models: Record<string, { id: string; name: string; limit?: { context?: number; output?: number } }>
  }[]
  default: Record<string, string>
}

export const api = {
  health: () => request<{ healthy: boolean; version: string }>("GET", "/global/health"),
  projects: () => request<Project[]>("GET", "/project"),
  vcs: (directory: string) => request<{ branch?: string; default_branch?: string }>("GET", "/vcs", { directory }),
  path: (directory?: string) =>
    request<{ home: string; state: string; config: string; worktree: string; directory: string }>("GET", "/path", {
      directory,
    }),

  // Sessions
  sessions: (directory: string) => request<Session[]>("GET", "/session", { directory }),
  sessionStatus: (directory: string) => request<Record<string, SessionStatus>>("GET", "/session/status", { directory }),
  session: (id: string) => request<Session>("GET", `/session/${id}`),
  createSession: (directory: string, body: { title?: string; parentID?: string } = {}) =>
    request<Session>("POST", "/session", { directory, body }),
  updateSession: (id: string, body: { title?: string }) => request<Session>("PATCH", `/session/${id}`, { body }),
  deleteSession: (id: string) => request<boolean>("DELETE", `/session/${id}`),
  messages: (id: string, opts: { limit?: number; before?: string } = {}) =>
    request<MessageWithParts[]>("GET", `/session/${id}/message`, { query: opts }),
  todos: (id: string) => request<Todo[]>("GET", `/session/${id}/todo`),
  /** TS answers [] without a messageID; a turn's diff is its user message's `summary.diffs`. */
  diff: (id: string, messageID?: string) =>
    request<SnapshotFileDiff[]>("GET", `/session/${id}/diff`, { query: { messageID } }),
  /** Uncommitted changes in the working tree (git projects). */
  vcsDiff: (directory: string) => request<VcsFileDiff[]>("GET", "/vcs/diff", { directory, query: { mode: "git" } }),
  promptAsync: (id: string, body: PromptInput) => request<void>("POST", `/session/${id}/prompt_async`, { body }),
  command: (id: string, body: { command: string; arguments: string; agent?: string; model?: string }) =>
    request<MessageWithParts>("POST", `/session/${id}/command`, { body }),
  abort: (id: string) => request<boolean>("POST", `/session/${id}/abort`),
  fork: (id: string, messageID?: string) => request<Session>("POST", `/session/${id}/fork`, { body: { messageID } }),
  revert: (id: string, messageID: string, partID?: string) =>
    request<Session>("POST", `/session/${id}/revert`, { body: { messageID, partID } }),
  unrevert: (id: string) => request<Session>("POST", `/session/${id}/unrevert`),
  summarize: (id: string, body: { providerID: string; modelID: string }) =>
    request<boolean>("POST", `/session/${id}/summarize`, { body }),

  // Waiting-on-you
  permissions: (directory: string) => request<PermissionRequest[]>("GET", "/permission", { directory }),
  replyPermission: (id: string, reply: PermissionReply, message?: string, directory?: string) =>
    request<boolean>("POST", `/permission/${id}/reply`, { directory, body: { reply, message } }),
  questions: (directory: string) => request<QuestionRequest[]>("GET", "/question", { directory }),
  replyQuestion: (id: string, answers: string[][], directory?: string) =>
    request<boolean>("POST", `/question/${id}/reply`, { directory, body: { answers } }),
  rejectQuestion: (id: string, directory?: string) => request<boolean>("POST", `/question/${id}/reject`, { directory }),

  // Settings
  globalConfig: () => request<Record<string, unknown>>("GET", "/global/config"),
  patchGlobalConfig: (body: Record<string, unknown>) =>
    request<Record<string, unknown>>("PATCH", "/global/config", { body }),
  providerCatalog: (directory?: string) =>
    request<{
      all: { id: string; name: string; source: string; env: string[]; models: Record<string, { name: string }> }[]
      default: Record<string, string>
      connected: string[]
    }>("GET", "/provider", { directory }),
  setAuth: (providerID: string, key: string) =>
    request<boolean>("PUT", `/auth/${providerID}`, { body: { type: "api", key } }),
  removeAuth: (providerID: string) => request<boolean>("DELETE", `/auth/${providerID}`),
  /** Sign-in methods per provider (plugins); a provider without an entry takes an API key. */
  authMethods: (directory?: string) => request<Record<string, AuthMethod[]>>("GET", "/provider/auth", { directory }),
  oauthAuthorize: (providerID: string, method: number, directory?: string) =>
    request<OAuthAuthorization | null | undefined>("POST", `/provider/${providerID}/oauth/authorize`, {
      directory,
      body: { method },
    }),
  /** Without a code this waits for the browser leg to finish (the "auto" flow). */
  oauthCallback: (
    providerID: string,
    method: number,
    opts: { code?: string; directory?: string; signal?: AbortSignal },
  ) =>
    request<boolean>("POST", `/provider/${providerID}/oauth/callback`, {
      directory: opts.directory,
      signal: opts.signal,
      body: { method, code: opts.code },
    }),
  mcpStatus: (directory?: string) =>
    request<Record<string, { status: string; error?: string }>>("GET", "/mcp", { directory }),
  mcpConnect: (name: string, directory?: string) => request<boolean>("POST", `/mcp/${name}/connect`, { directory }),
  mcpDisconnect: (name: string, directory?: string) =>
    request<boolean>("POST", `/mcp/${name}/disconnect`, { directory }),

  // Catalog
  providers: (directory?: string) => request<ProviderList>("GET", "/config/providers", { directory }),
  agents: (directory?: string) => request<Agent[]>("GET", "/agent", { directory }),
  commands: (directory?: string) => request<Command[]>("GET", "/command", { directory }),
  listFiles: (directory: string, path: string) =>
    request<{ name: string; path: string; absolute: string; type: "file" | "directory"; ignored: boolean }[]>(
      "GET",
      "/file",
      { directory, query: { path } },
    ),
  readFile: (directory: string, path: string) =>
    request<{ type: "text" | "binary"; content: string; encoding?: "base64"; mimeType?: string }>(
      "GET",
      "/file/content",
      { directory, query: { path } },
    ),
  findFiles: (directory: string, query: string, limit = 20) =>
    request<string[]>("GET", "/find/file", { directory, query: { query, limit } }),
}

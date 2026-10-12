// A transcript as Markdown, for "Copy" on a reply and "Export" on a session.
// Prose goes as written; a tool call is one line (name, target, status);
// reasoning and the server's synthetic notes are left out.
import type { AssistantMessage, Part, ToolPart, UserMessage } from "../../api/types"
import type { State } from "../../store/state"

const synthetic = (p: Part) => p.type === "text" && !!(p as { synthetic?: boolean }).synthetic

function toolLine(p: ToolPart) {
  const st = p.state as { status: string; title?: string; input?: Record<string, unknown> }
  const input = st.input ?? {}
  const target = st.title || String(input.filePath ?? input.command ?? input.pattern ?? input.url ?? "")
  const status = st.status === "completed" ? "" : ` (${st.status})`
  return `- \`${p.tool}\`${target ? ` ${target.split("\n")[0]}` : ""}${status}`
}

/** The text of one message's parts, as Markdown. */
export function messageMarkdown(s: State, messageID: string, opts: { tools?: boolean } = {}) {
  const out: string[] = []
  for (const id of s.parts[messageID] ?? []) {
    const p = s.part[id]
    if (!p || synthetic(p)) continue
    if (p.type === "text") out.push((p as { text: string }).text.trim())
    else if (p.type === "file") out.push(`[file: ${(p as { filename?: string }).filename ?? "attachment"}]`)
    else if (p.type === "tool" && opts.tools) out.push(toolLine(p as ToolPart))
  }
  return out.filter(Boolean).join("\n\n")
}

/** A reply (every assistant message of one turn) as Markdown, prose only: what "Copy" puts on the clipboard. */
export const replyMarkdown = (s: State, messageIDs: string[]) =>
  messageIDs
    .map((id) => messageMarkdown(s, id))
    .filter(Boolean)
    .join("\n\n")

const stamp = (ms: number) => new Date(ms).toISOString().replace("T", " ").slice(0, 16)

/** The whole loaded transcript, with headings per message. */
export function transcriptMarkdown(s: State, sessionID: string) {
  const session = s.sessions[sessionID]
  const out = [`# ${session?.title || sessionID}`, ""]
  if (session) out.push(`${session.directory} · ${sessionID}`, "")
  let lastAgent = ""
  for (const id of s.messages[sessionID] ?? []) {
    const m = s.message[id]
    if (!m) continue
    const body = messageMarkdown(s, id, { tools: true })
    if (m.role === "user") {
      lastAgent = ""
      if (!body) continue
      out.push(`## You · ${stamp((m as UserMessage).time.created)}`, "", body, "")
    } else {
      const a = m as AssistantMessage
      const head = `${a.agent} · ${a.modelID}`
      // One heading per reply, however many steps it took.
      if (head !== lastAgent) out.push(`## ${head} · ${stamp(a.time.created)}`, "")
      lastAgent = head
      if (body) out.push(body, "")
      if (a.error && a.error.name !== "MessageAbortedError") out.push(`> **${a.error.name}**`, "")
    }
  }
  return out.join("\n").trimEnd() + "\n"
}

/** A file name from the title: letters, digits and dashes. */
export const exportName = (title: string) =>
  `${
    title
      .toLowerCase()
      .replace(/[^a-z0-9]+/g, "-")
      .replace(/^-|-$/g, "")
      .slice(0, 60) || "transcript"
  }.md`

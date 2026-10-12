// Review comments: notes on lines of a diff (Changes) or a file (Files),
// held per session and sent with the next prompt as one text part that
// quotes each line, as upstream's line comments do.
import { createStore } from "solid-js/store"

export interface LineComment {
  id: string
  file: string
  line: number
  /** `old` for a deleted line in a diff; otherwise the file as it is now. */
  side: "new" | "old"
  /** The commented line's text, quoted so the model sees what was meant. */
  code: string
  text: string
}

const [comments, setComments] = createStore<Record<string, LineComment[]>>({})
export { comments }

let seq = 0
export function addComment(sessionID: string, c: Omit<LineComment, "id">) {
  setComments(sessionID, (list) => [...(list ?? []), { ...c, id: `cm_${Date.now()}_${seq++}` }])
}
export const removeComment = (sessionID: string, id: string) =>
  setComments(sessionID, (list) => (list ?? []).filter((c) => c.id !== id))
export const clearComments = (sessionID: string) => setComments(sessionID, [])

export const commentsAt = (list: LineComment[] | undefined, file: string, line: number, side: "new" | "old") =>
  (list ?? []).filter((c) => c.file === file && c.line === line && c.side === side)

/** The prompt text for a set of comments, in file then line order. */
export function commentsText(list: LineComment[]) {
  if (!list.length) return ""
  const sorted = [...list].sort((a, b) => a.file.localeCompare(b.file) || a.line - b.line)
  const lines = sorted.map((c) => {
    const where = `${c.file}:${c.line}${c.side === "old" ? " (removed line)" : ""}`
    const code = c.code.trim()
    return `- ${where}${code ? ` \`${code.replace(/`/g, "'")}\`` : ""}\n  ${c.text.trim().replace(/\n/g, "\n  ")}`
  })
  return `Review comments on specific lines:\n${lines.join("\n")}`
}

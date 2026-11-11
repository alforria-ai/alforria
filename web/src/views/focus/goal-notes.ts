// How the goal loop (`/goal`) talks to the model, read back for a person: a
// continuation is a prompt nobody typed, so the transcript shows it as a note.
import type { Goal } from "../../api/types"
import type { FleetState } from "../../fleet/derive"

export interface GoalNote {
  /** The user resumed a paused goal: there was no review before this round. */
  resumed: boolean
  /** The reviewer's notes, one per line (its reason, then any next step). */
  feedback: string[]
}

const CONTINUATION = "Continue working toward the active goal."

/** Reviewer notes arrive with `&`, `<` and `>` escaped; a person should read them as written. */
const unescape = (text: string) => text.replaceAll("&lt;", "<").replaceAll("&gt;", ">").replaceAll("&amp;", "&")

/** Recognize a synthetic text part as a goal continuation. */
export function goalNoteOf(text: string): GoalNote | undefined {
  if (!text.startsWith(CONTINUATION)) return undefined
  const block = /<reviewer_feedback>\n?([\s\S]*?)\n?<\/reviewer_feedback>/.exec(text)?.[1] ?? ""
  const feedback = block
    .split("\n")
    .map((line) => unescape(line).trim())
    .filter(Boolean)
  return { resumed: !feedback.length && text.includes("\nThe user resumed the goal."), feedback }
}

/** The note's one sentence, before any reviewer notes. */
export const goalNoteLabel = (note: GoalNote) => (note.resumed ? "Goal: resumed" : "Goal: continuing")

/** Lamp and word for a goal: active is in progress, paused wants a look, complete is done. */
export function goalState(goal: Pick<Goal, "status">): { lamp: FleetState; word: string } {
  if (goal.status === "active") return { lamp: "working", word: "Goal active" }
  if (goal.status === "paused") return { lamp: "retry", word: "Goal paused" }
  return { lamp: "idle", word: "Goal complete" }
}

/** The slash-command arguments a goal's controls may send, in display order. */
export function goalActions(goal: Pick<Goal, "status">): ("pause" | "resume" | "clear")[] {
  if (goal.status === "active") return ["pause"]
  if (goal.status === "paused") return ["resume", "clear"]
  return ["clear"]
}

// How the system talks about subagents in text meant for the model, read back
// for a person.
export type Note =
  { kind: "update"; session: string; state: string; summary: string } | { kind: "resume"; error: string }

export const firstLine = (text: string) =>
  text
    .split("\n")
    .find((line) => line.trim())
    ?.trim() ?? ""

/** The note's summary is written for the model; say it to the person. */
function updateSummary(summary: string) {
  const direct = /^The user continued this subagent directly: (.*)$/.exec(summary)
  if (direct) return `You continued ${shortTitle(direct[1]!)}. The main agent has its result.`
  return shortTitle(summary)
}

/** Recognize a synthetic text part as one of the notes. */
export function noteOf(text: string): Note | undefined {
  if (text.startsWith("<auto_resume>")) {
    const error = /stopped with an error: ([\s\S]*?)\n\n/.exec(text)?.[1]
    return error === undefined ? undefined : { kind: "resume", error: firstLine(error) }
  }
  const update = /^<task id="([^"]+)" state="([^"]+)">\s*<summary>([\s\S]*?)<\/summary>/.exec(text)
  if (update) return { kind: "update", session: update[1]!, state: update[2]!, summary: updateSummary(update[3]!) }
  return undefined
}

/** A tool error as a person should read it: the server may store a serialized `{ name, data: { message } }`. */
export function toolErrorText(error: string): string {
  try {
    const message = (JSON.parse(error) as { data?: { message?: unknown } })?.data?.message
    if (typeof message === "string") return message
  } catch {
    // Plain words already.
  }
  return error
}

/** The one line that says why a subagent failed, without the instructions the error carries for the model. */
export function failureLine(error: string): string {
  const line = firstLine(toolErrorText(error))
  return /^Subagent failed \(task_id: [^)]*\): (.*)$/.exec(line)?.[1] ?? line
}

/** `Research (@general subagent)` → `Research`. */
export const shortTitle = (title: string) => title.replace(/ \(@[^)]*\)$/, "")

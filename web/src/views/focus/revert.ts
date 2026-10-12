// Session revert, as TS has it: `session.revert.messageID` marks the first
// rolled-back message. Nothing is deleted until the next prompt, which
// discards everything from that message on; Unrevert brings it all back.
// The server refuses either while a turn runs, so a busy session is stopped
// first (upstream `halt`).
import { api } from "../../api/client"
import type { State } from "../../store/state"
import { state } from "../../store/store"

/** The first rolled-back message id, or undefined when the session isn't reverted. */
export const revertBoundary = (s: State, sessionID: string) => s.sessions[sessionID]?.revert?.messageID

/** Message ids ascend with creation, so "at or after the boundary" is an id comparison. */
export function isReverted(s: State, sessionID: string, messageID: string) {
  const at = revertBoundary(s, sessionID)
  return !!at && messageID >= at
}

/** A user message's own text (synthetic parts are the server's, not the prompt). */
export function promptText(s: State, messageID: string) {
  return (s.parts[messageID] ?? [])
    .map((id) => s.part[id])
    .filter((p): p is Extract<NonNullable<typeof p>, { type: "text" }> => p?.type === "text" && !p.synthetic)
    .map((p) => p.text)
    .join("\n")
}

/** The rolled-back prompts, oldest first. */
export function revertedPrompts(s: State, sessionID: string): { id: string; text: string }[] {
  const at = revertBoundary(s, sessionID)
  if (!at) return []
  return (s.messages[sessionID] ?? [])
    .filter((id) => id >= at && s.message[id]?.role === "user")
    .map((id) => ({ id, text: promptText(s, id) }))
}

/** Whether the server can put files back: snapshots need git. */
export const canRestoreFiles = (s: State, sessionID: string) =>
  s.projects[s.sessions[sessionID]?.projectID ?? ""]?.vcs === "git"

export const isBusy = (s: State, sessionID: string) => {
  const t = s.status[sessionID]?.type
  return t === "busy" || t === "retry"
}

/** Stop a running turn and wait (up to 10 s) for the session to settle. */
async function halt(sessionID: string) {
  if (!isBusy(state, sessionID)) return
  await api.abort(sessionID)
  for (let waited = 0; isBusy(state, sessionID) && waited < 10_000; waited += 100)
    await new Promise((r) => setTimeout(r, 100))
}

export const revertTo = (sessionID: string, messageID: string) =>
  halt(sessionID).then(() => api.revert(sessionID, messageID))

export const unrevert = (sessionID: string) => halt(sessionID).then(() => api.unrevert(sessionID))

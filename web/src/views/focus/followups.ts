// Follow-ups queued while a session works: each waits client-side and goes
// out, oldest first, once the session is idle. Enter still sends at once
// (the server folds it into the running turn, upstream's "steer"); ⌥↵ queues
// instead. Stopping a turn pauses the queue so the next item doesn't start
// on its own; Send now (or any send) resumes it. A failed send stops the
// queue on that item until the human acts.
import { createEffect, createRoot } from "solid-js"
import { createStore } from "solid-js/store"
import { api } from "../../api/client"
import { state } from "../../store/store"
import { toast } from "../toast"
import { promptBody, type Draft } from "./prompt"
import { isBusy } from "./revert"

export interface Followup {
  id: string
  body: Draft
}

const [queue, setQueue] = createStore<{
  items: Record<string, Followup[]>
  paused: Record<string, boolean>
  failed: Record<string, string | undefined>
  /** A send in flight (or waiting for the session to report busy). */
  sending: Record<string, boolean>
}>({ items: {}, paused: {}, failed: {}, sending: {} })
export { queue }

let seq = 0
export function enqueue(sessionID: string, body: Draft) {
  setQueue("items", sessionID, (list) => [...(list ?? []), { id: `fu_${Date.now()}_${seq++}`, body }])
  setQueue("paused", sessionID, false)
}

/** Take an item out of the queue (to edit or drop it). */
export function take(sessionID: string, id: string) {
  const item = queue.items[sessionID]?.find((f) => f.id === id)
  setQueue("items", sessionID, (list) => (list ?? []).filter((f) => f.id !== id))
  if (queue.failed[sessionID] === id) setQueue("failed", sessionID, undefined)
  return item
}

export const pause = (sessionID: string) => {
  if (queue.items[sessionID]?.length) setQueue("paused", sessionID, true)
}
export const resume = (sessionID: string) => setQueue("paused", sessionID, false)

/** Send one item now, whatever the session is doing; this also resumes a paused queue. */
export async function sendNow(sessionID: string, id: string) {
  setQueue("paused", sessionID, false)
  await send(sessionID, id)
}

async function send(sessionID: string, id: string) {
  const item = queue.items[sessionID]?.find((f) => f.id === id)
  if (!item || queue.sending[sessionID]) return
  setQueue("sending", sessionID, true)
  setQueue("failed", sessionID, undefined)
  try {
    await api.promptAsync(sessionID, promptBody(item.body))
    take(sessionID, id)
    // The prompt is accepted before the session reports busy; don't start
    // the next item in that gap.
    for (let waited = 0; !isBusy(state, sessionID) && waited < 3000; waited += 100)
      await new Promise((r) => setTimeout(r, 100))
  } catch (err) {
    setQueue("failed", sessionID, id)
    toast(`Could not send the queued follow-up: ${err instanceof Error ? err.message : err}`, "error")
  } finally {
    setQueue("sending", sessionID, false)
  }
}

// The drain: whenever a session with a queue is idle, send its oldest item.
createRoot(() =>
  createEffect(() => {
    for (const sid of Object.keys(queue.items)) {
      const next = queue.items[sid]?.[0]
      if (!next || queue.paused[sid] || queue.sending[sid] || queue.failed[sid] === next.id) continue
      if (isBusy(state, sid)) continue
      void send(sid, next.id)
    }
  }),
)

/** The first line of a queued prompt, for the dock. */
export const followupLine = (f: Followup) =>
  f.body.text.split("\n").find((l) => l.trim()) ?? `[${f.body.attachments.length} attachments]`

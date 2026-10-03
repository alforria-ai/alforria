// Acting on a waiting item: one code path for the queue and the inline slips
// in transcripts. The verdict stamp has to outlive the record: the server's
// `permission.replied` usually lands before the stamp has been seen, so acted
// items are held here for the stamp + fold and merged back into the list.
import { createRoot, createSignal } from "solid-js"
import { createStore, produce } from "solid-js/store"
import { api } from "../../api/client"
import type { PermissionReply } from "../../api/types"
import type { Interrupt } from "../../fleet/derive"
import { state } from "../../store/store"
import { clock } from "../../ui/format"
import { toast } from "../toast"

export const STAMP_HOLD = 480
export const FOLD = 200

export type Verdict = PermissionReply | "answer" | "dismiss"

export interface Stamped {
  interrupt: Interrupt
  verdict: Verdict
  label: string
  time: string
  folding: boolean
}

export interface Cleared {
  time: string
  label: string
  what: string
}

export const [stamped, setStamped] = createStore<Record<string, Stamped>>({})
export const [ledger, setLedger] = createStore<Cleared[]>([])
/** Which queue item is expanded; null means "the oldest". */
export const [selected, setSelected] = createRoot(() => createSignal<string | null>(null))

export function verdictLabel(i: Interrupt, v: Verdict) {
  const doom = i.kind === "permission" && i.request.permission === "doom_loop"
  switch (v) {
    case "once":
      return doom ? "Continued" : "Allowed once"
    case "always":
      return "Always allowed"
    case "reject":
      return doom ? "Stopped" : "Denied"
    case "answer":
      return "Answered"
    case "dismiss":
      return "Dismissed"
  }
}

function directoryOf(sessionID: string) {
  return state.sessions[sessionID]?.directory
}

/**
 * Reply to a permission or question. Stamps immediately, sends the reply, then
 * folds the item away. On failure the stamp is withdrawn and the item stays.
 */
export async function act(
  i: Interrupt,
  verdict: Verdict,
  extra: { answers?: string[][]; message?: string; what?: string } = {},
) {
  if (stamped[i.id]) return
  const label = verdictLabel(i, verdict)
  const time = clock(Date.now())
  setStamped(i.id, { interrupt: i, verdict, label, time, folding: false })
  const dir = directoryOf(i.sessionID)
  try {
    if (i.kind === "permission") await api.replyPermission(i.id, verdict as PermissionReply, extra.message, dir)
    else if (verdict === "dismiss") await api.rejectQuestion(i.id, dir)
    else await api.replyQuestion(i.id, extra.answers ?? [], dir)
  } catch (err) {
    setStamped(produce((s) => delete s[i.id]))
    toast(`Could not send “${label}”: ${err instanceof Error ? err.message : String(err)}`, "error")
    return
  }
  setLedger(
    produce((l) => {
      l.unshift({ time: time.slice(0, 5), label, what: extra.what ?? "" })
      if (l.length > 50) l.length = 50
    }),
  )
  if (selected() === i.id) setSelected(null)
  setTimeout(() => setStamped(i.id, "folding", true), STAMP_HOLD)
  setTimeout(() => setStamped(produce((s) => delete s[i.id])), STAMP_HOLD + FOLD + 40)
}

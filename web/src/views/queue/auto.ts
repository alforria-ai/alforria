// Per-session auto-accept, as upstream's (context/permission.tsx): while this
// page is open, every permission a chosen session (or a subagent under it)
// asks for is allowed once, from here, and logged in the cleared ledger as
// "Auto-allowed". Questions and doom-loop stops still wait for the human.
// Turning it on goes through Confirm-to-Loosen; turning it off applies at once.
import { createEffect, createRoot } from "solid-js"
import { createStore, produce } from "solid-js/store"
import { displayTitle, interrupts } from "../../fleet/fleet"
import { state } from "../../store/store"
import { act } from "./act"
import { slipSummary } from "./slip"

const KEY = "alforria.autoAccept"

function stored(): Record<string, true> {
  try {
    const v = JSON.parse(localStorage.getItem(KEY) ?? "{}")
    return v && typeof v === "object" ? v : {}
  } catch {
    return {}
  }
}

const [accepting, setAccepting] = createStore<Record<string, true>>(typeof window === "undefined" ? {} : stored())

function save() {
  try {
    localStorage.setItem(KEY, JSON.stringify(accepting))
  } catch {
    /* it just won't survive a reload */
  }
}

/** Set on this session itself (not inherited from a parent). */
export const autoAcceptOwn = (sessionID: string) => !!accepting[sessionID]

/** The session, or the session a subagent runs under, auto-accepts. */
export function autoAccepting(sessionID: string) {
  const seen = new Set<string>()
  for (let id: string | undefined = sessionID; id && !seen.has(id); id = state.sessions[id]?.parentID) {
    if (accepting[id]) return true
    seen.add(id)
  }
  return false
}

export function setAutoAccept(sessionID: string, on: boolean) {
  setAccepting(produce((a) => (on ? (a[sessionID] = true) : delete a[sessionID])))
  save()
}

/** Answer what auto-accepting sessions ask, each request once. */
export function startAutoAccept() {
  // A request whose reply failed stays in the queue for the human.
  const tried = new Set<string>()
  createRoot(() =>
    createEffect(() => {
      for (const i of interrupts()) {
        if (i.kind !== "permission" || i.request.permission === "doom_loop" || tried.has(i.id)) continue
        if (!autoAccepting(i.sessionID)) continue
        tried.add(i.id)
        void act(i, "once", { auto: true, what: `${displayTitle(state.sessions[i.sessionID])}: ${slipSummary(i)}` })
      }
    }),
  )
}

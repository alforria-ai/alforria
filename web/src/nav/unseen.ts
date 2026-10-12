// Unseen outcomes: a top-level session that finished a turn or failed while
// nobody was looking at it. It stays marked in the table and the session list
// until it is shown in a focus column with the page visible; U opens the
// oldest. Waiting sessions need no marker: the queue already holds them.
import { createEffect, createRoot, createSignal } from "solid-js"
import { createStore, produce } from "solid-js/store"
import { fleetState, type FleetState } from "../fleet/derive"
import { state } from "../store/store"
import { nav } from "./route"

export type Outcome = "done" | "fault"

const [unseen, setUnseen] = createStore<Record<string, { outcome: Outcome; at: number }>>({})
export { unseen }

/** Unseen sessions still in the store, oldest outcome first. */
export function unseenOrder() {
  return Object.entries(unseen)
    .filter(([id]) => state.sessions[id] && !state.sessions[id]!.time.archived)
    .sort(([, a], [, b]) => a.at - b.at)
    .map(([id]) => id)
}

const [visible, setVisible] = createRoot(() =>
  createSignal(typeof document === "undefined" || document.visibilityState === "visible"),
)

function shown(id: string) {
  return visible() && nav.view === "focus" && nav.columns.some((c) => c.kind === "session" && c.session === id)
}

/**
 * Watch top-level sessions for turns that end: working → idle is done, any
 * state → fault is a failure. `onOutcome` hears every one (for OS
 * notifications); only those nobody is viewing are marked unseen.
 */
export function watchOutcomes(onOutcome: (sessionID: string, outcome: Outcome) => void) {
  if (typeof document !== "undefined")
    document.addEventListener("visibilitychange", () => setVisible(document.visibilityState === "visible"))
  createRoot(() => {
    let prev: Map<string, FleetState> | null = null
    createEffect(() => {
      if (!state.synced) return
      const next = new Map<string, FleetState>()
      for (const s of Object.values(state.sessions)) if (!s.parentID) next.set(s.id, fleetState(state, s.id))
      if (prev)
        for (const [id, st] of next) {
          const was = prev.get(id)
          if (!was || was === st) continue
          const outcome: Outcome | null =
            st === "fault" ? "fault" : st === "idle" && (was === "working" || was === "retry") ? "done" : null
          if (!outcome) continue
          if (!shown(id)) setUnseen(id, { outcome, at: Date.now() })
          onOutcome(id, outcome)
        }
      prev = next
    })
    // Seen once it is on screen; a session that leaves the store goes too.
    createEffect(() => {
      for (const id of Object.keys(unseen))
        if (shown(id) || !state.sessions[id]) setUnseen(produce((u) => delete u[id]))
    })
  })
}

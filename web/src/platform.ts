// The platform layer: things a desktop shell (Tauri) would do natively and the
// browser does through web APIs. Keep every call site going through here so a
// shell can swap the implementation.
import { createEffect, createRoot, createSignal, on } from "solid-js"
import { interrupts } from "./fleet/fleet"
import { state } from "./store/store"

const KEY = "alforria.notify"

function stored() {
  try {
    return localStorage.getItem(KEY) === "on"
  } catch {
    return false
  }
}

const [notifyOn, setNotifyOn] = createRoot(() => createSignal(typeof window !== "undefined" && stored()))
export { notifyOn }

export const canNotify = () => typeof Notification !== "undefined"

/** Ask once (from a user gesture) and remember the choice in this browser. */
export async function enableNotifications(on: boolean) {
  if (on && canNotify() && Notification.permission !== "granted") {
    const result = await Notification.requestPermission()
    if (result !== "granted") on = false
  }
  setNotifyOn(on)
  try {
    localStorage.setItem(KEY, on ? "on" : "off")
  } catch {
    /* preference just won't persist */
  }
  return on
}

function notify(title: string, body: string, onClick: () => void) {
  if (!notifyOn() || !canNotify() || Notification.permission !== "granted") return
  const n = new Notification(title, { body, tag: title, silent: false })
  n.onclick = () => {
    window.focus()
    onClick()
    n.close()
  }
}

/**
 * Background signals: the tab title carries the waiting count, and a new
 * waiting item raises an OS notification while the tab is hidden.
 */
export function startPlatform(open: (sessionID: string) => void) {
  createRoot(() => {
    createEffect(() => {
      const n = interrupts().length
      document.title = n ? `(${n}) alforria` : "alforria"
    })
    let known = new Set(interrupts().map((i) => i.id))
    createEffect(
      on(interrupts, (list) => {
        const ids = new Set(list.map((i) => i.id))
        if (document.hidden)
          for (const i of list) {
            if (known.has(i.id)) continue
            const title = state.sessions[i.sessionID]?.title || "A session"
            const what =
              i.kind === "question"
                ? (i.request.questions[0]?.question ?? "has a question")
                : `wants to run ${i.request.permission}`
            notify(`${title} is waiting on you`, what, () => open(i.sessionID))
          }
        known = ids
      }),
    )
  })
}

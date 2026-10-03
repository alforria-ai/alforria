// Transient notices for failures the user should know about (a reply that did
// not send, a lost connection). Success is shown in place, never here.
import { For } from "solid-js"
import { createStore, produce } from "solid-js/store"

interface Toast {
  id: number
  text: string
  tone: "info" | "error"
}

const [toasts, setToasts] = createStore<Toast[]>([])
let seq = 0

export function toast(text: string, tone: Toast["tone"] = "info", ms = 6000) {
  const id = ++seq
  setToasts(produce((t) => t.push({ id, text, tone })))
  setTimeout(
    () =>
      setToasts(
        produce((t) => {
          const at = t.findIndex((x) => x.id === id)
          if (at >= 0) t.splice(at, 1)
        }),
      ),
    ms,
  )
}

export function Toasts() {
  return (
    <div class="toasts" role="status" aria-live="polite">
      <For each={toasts}>{(t) => <div class={`toast ${t.tone}`}>{t.text}</div>}</For>
    </div>
  )
}

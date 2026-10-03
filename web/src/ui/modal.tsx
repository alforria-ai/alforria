// Modal dialogs: one frame for the palette, the keys sheet and provider
// sign-in. While one is open the app behind it is inert (no focus, clicks or
// screen-reader access), the global keymap stands down, Tab wraps inside the
// dialog, Esc or a press on the scrim closes it, and focus returns to what had
// it before — unless the dialog's action moved focus somewhere on purpose.
import { createRoot, createSignal, onCleanup, onMount, type JSX } from "solid-js"
import { Portal } from "solid-js/web"
import { Icon } from "./icons"

const [depth, setDepth] = createRoot(() => createSignal(0))
/** True while any modal is open; the global keymap checks it. */
export const modalOpen = () => depth() > 0

const FOCUSABLE =
  'a[href],button:not([disabled]),input:not([disabled]):not([type="hidden"]),select:not([disabled]),textarea:not([disabled]),[tabindex]:not([tabindex="-1"])'

/** Captured when the first modal opens, restored when the last one closes. */
let returnTo: HTMLElement | null = null

let seq = 0
export const dialogId = () => `dlg-${++seq}`

export function Modal(props: {
  /** Id of the element naming the dialog (a DialogHead title, or the palette input). */
  labelledBy?: string
  label?: string
  class?: string
  onClose: () => void
  /** First focus; defaults to the first focusable control, then the panel. */
  initialFocus?: (panel: HTMLElement) => HTMLElement | undefined
  /** Sees every key first; preventDefault() to keep the frame from acting on it. */
  onKey?: (e: KeyboardEvent) => void
  children: JSX.Element
}) {
  let panel!: HTMLDivElement
  if (depth() === 0 && !returnTo) returnTo = document.activeElement as HTMLElement | null
  setDepth((d) => d + 1)
  const app = document.getElementById("app")
  app?.setAttribute("inert", "")

  onMount(() => {
    const first = props.initialFocus?.(panel) ?? panel.querySelector<HTMLElement>(FOCUSABLE) ?? panel
    first.focus({ preventScroll: true })
  })
  onCleanup(() => {
    setDepth((d) => d - 1)
    // Defer: one modal may hand over to another (palette → keys sheet).
    queueMicrotask(() => {
      if (depth() > 0) return
      app?.removeAttribute("inert")
      const target = returnTo
      returnTo = null
      // An action that navigated or focused something else wins.
      const moved = document.activeElement && document.activeElement !== document.body
      if (!moved && target?.isConnected) target.focus({ preventScroll: true })
    })
  })

  const onKeyDown = (e: KeyboardEvent) => {
    props.onKey?.(e)
    if (e.key === "Escape" && !e.defaultPrevented) {
      e.preventDefault()
      e.stopPropagation()
      return props.onClose()
    }
    if (e.key !== "Tab") return
    const list = [...panel.querySelectorAll<HTMLElement>(FOCUSABLE)].filter((el) => el.offsetParent !== null)
    if (!list.length) return e.preventDefault()
    const first = list[0]!
    const last = list[list.length - 1]!
    if (e.shiftKey && (document.activeElement === first || document.activeElement === panel)) {
      e.preventDefault()
      last.focus()
    } else if (!e.shiftKey && document.activeElement === last) {
      e.preventDefault()
      first.focus()
    }
  }

  return (
    <Portal>
      <div
        class="scrim"
        onMouseDown={(e) => {
          if (e.target === e.currentTarget) {
            e.preventDefault()
            props.onClose()
          }
        }}
      >
        <div
          ref={panel}
          class={`dialog ${props.class ?? ""}`}
          role="dialog"
          aria-modal="true"
          aria-labelledby={props.labelledBy}
          aria-label={props.labelledBy ? undefined : props.label}
          tabindex="-1"
          onKeyDown={onKeyDown}
        >
          {props.children}
        </div>
      </div>
    </Portal>
  )
}

/** Title row with a close control (touch has no Esc). */
export function DialogHead(props: { id: string; title: string; onClose: () => void; children?: JSX.Element }) {
  return (
    <header class="dialog-head">
      <h2 id={props.id}>{props.title}</h2>
      {props.children}
      <button class="dialog-x" aria-label="Close" title="Close (Esc)" onClick={props.onClose}>
        <Icon name="close" />
      </button>
    </header>
  )
}

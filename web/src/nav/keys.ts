// The global keyboard map. Keys act only when focus is not in a text field
// (except Esc, ⌘K and Alt+↑/↓, which work everywhere). Region-specific keys
// (the queue's A/S/D, a column's composer) are handled by those regions.

export interface KeyActions {
  palette(): void
  keys(): void
  back(): void
  focusView(): void
  queue(): void
  nextWaiting(): void
  newSession(): void
  cursor(delta: number): void
  openCursor(beside: boolean): void
  column(delta: number): void
  stepSession(delta: number): void
  compose(): void
}

export function isTyping(el: Element | null) {
  if (!el) return false
  const t = el.tagName
  return t === "INPUT" || t === "TEXTAREA" || t === "SELECT" || (el as HTMLElement).isContentEditable
}

export function bindKeys(view: () => "overview" | "focus" | "settings", a: KeyActions) {
  const onKey = (e: KeyboardEvent) => {
    if (e.defaultPrevented) return
    const k = e.key
    if ((e.metaKey || e.ctrlKey) && k.toLowerCase() === "k") {
      e.preventDefault()
      return a.palette()
    }
    if (e.altKey && (k === "ArrowDown" || k === "ArrowUp") && view() === "focus") {
      e.preventDefault()
      return a.stepSession(k === "ArrowDown" ? 1 : -1)
    }
    const typing = isTyping(document.activeElement)
    if (k === "Escape") {
      if (typing) return (document.activeElement as HTMLElement).blur()
      return a.back()
    }
    if (typing || e.metaKey || e.ctrlKey || e.altKey) return
    // Keys that region handlers already consumed (queue slips) stop here.
    const inQueue = (document.activeElement as HTMLElement | null)?.closest?.(".queue:not(.rail)")
    const lower = k.toLowerCase()
    const go = (fn: () => void) => {
      e.preventDefault()
      fn()
    }
    if (k === "?") return go(a.keys)
    if (k === "/") return go(a.palette)
    if (lower === "q") return go(a.queue)
    if (lower === "n") return go(a.newSession)
    if (inQueue) return
    if (view() === "overview") {
      if (lower === "j" || k === "ArrowDown") return go(() => a.cursor(1))
      if (lower === "k" || k === "ArrowUp") return go(() => a.cursor(-1))
      if (k === "Enter") return go(() => a.openCursor(e.shiftKey))
      if (lower === "f") return go(a.focusView)
    }
    if (view() === "focus") {
      if (lower === "w") return go(a.nextWaiting)
      if (k === "[") return go(() => a.column(-1))
      if (k === "]") return go(() => a.column(1))
      if (lower === "i") return go(a.compose)
    }
  }
  document.addEventListener("keydown", onKey)
  return () => document.removeEventListener("keydown", onKey)
}

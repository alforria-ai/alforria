// One EventSource on /global/event for the whole app. The server heartbeats
// every 10 s; a half-open socket (laptop sleep, NAT drop) never errors, so a
// watchdog reconnects after 25 s of silence. Backoff mirrors the TUI: 1 s,
// doubling, capped at 30 s.
import type { GlobalEvent } from "../api/types"

export interface StreamHandlers {
  onFrame(frame: GlobalEvent): void
  /** Called on every successful (re)connect, before any frame is delivered. */
  onOpen(): void
  onState(state: "connecting" | "live" | "retrying", attempt: number, retryAt: number): void
}

export const HEARTBEAT_GRACE = 25_000
const MAX_BACKOFF = 30_000

export function backoff(attempt: number) {
  return Math.min(MAX_BACKOFF, 1000 * 2 ** Math.max(0, attempt - 1))
}

export function connectEvents(url: string, h: StreamHandlers): () => void {
  let es: EventSource | undefined
  let watchdog: ReturnType<typeof setTimeout> | undefined
  let retry: ReturnType<typeof setTimeout> | undefined
  let attempt = 0
  let closed = false

  const arm = () => {
    clearTimeout(watchdog)
    watchdog = setTimeout(restart, HEARTBEAT_GRACE)
  }

  const open = () => {
    h.onState(attempt ? "retrying" : "connecting", attempt, 0)
    es = new EventSource(url, { withCredentials: true })
    es.onopen = () => {
      attempt = 0
      h.onState("live", 0, 0)
      arm()
      h.onOpen()
    }
    es.onmessage = (m) => {
      arm()
      let frame: GlobalEvent
      try {
        frame = JSON.parse(m.data)
      } catch {
        return
      }
      if (frame?.payload?.type) h.onFrame(frame)
    }
    // EventSource would retry on its own with a fixed delay; take over so the
    // backoff and the resync-on-open stay ours.
    es.onerror = restart
  }

  function restart() {
    es?.close()
    es = undefined
    clearTimeout(watchdog)
    if (closed) return
    attempt++
    const delay = backoff(attempt)
    h.onState("retrying", attempt, Date.now() + delay)
    retry = setTimeout(open, delay)
  }

  open()
  return () => {
    closed = true
    es?.close()
    clearTimeout(watchdog)
    clearTimeout(retry)
  }
}

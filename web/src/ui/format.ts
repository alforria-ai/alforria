// Formatting for data cells. Mono, tabular, terse: these render in tables.
import { createSignal } from "solid-js"

/** A shared one-second clock so every age/countdown cell ticks together. */
const [now, setNow] = createSignal(Date.now())
if (typeof window !== "undefined") setInterval(() => setNow(Date.now()), 1000)
export { now }

export function age(ms: number) {
  const s = Math.max(0, Math.floor(ms / 1000))
  if (s < 60) return `${s}s`
  const m = Math.floor(s / 60)
  if (m < 60) return `${m}m`
  const h = Math.floor(m / 60)
  if (h < 24) return `${h}h`
  return `${Math.floor(h / 24)}d`
}

/** Wait time with seconds, for things the human is holding up. */
export function waited(ms: number) {
  const s = Math.max(0, Math.floor(ms / 1000))
  const m = Math.floor(s / 60)
  return m ? `${m}m ${String(s % 60).padStart(2, "0")}s` : `${s}s`
}

export function clock(t: number) {
  return new Date(t).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false })
}

export function hm(t: number) {
  return new Date(t).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", hour12: false })
}

export const money = (n: number) => `$${n.toFixed(2)}`

export function ktok(n: number) {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(2)}M`
  if (n >= 1000) return `${(n / 1000).toFixed(1)}k`
  return String(n)
}

export function basename(path: string) {
  const parts = path.replace(/\/+$/, "").split("/")
  return parts[parts.length - 1] || path
}

/** `~/…` for paths under the server's home directory. */
export function tildify(path: string, home?: string) {
  return home && path.startsWith(home) ? `~${path.slice(home.length)}` : path
}

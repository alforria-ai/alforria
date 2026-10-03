// The single application store. Frames from the event stream are queued and
// applied in one `produce` per 16 ms tick, so a burst of deltas costs one
// reactive update; Solid then re-renders only the text nodes that changed.
import { createStore, produce } from "solid-js/store"
import type { GlobalEvent } from "../api/types"
import { reduce } from "./reduce"
import { emptyState, type State } from "./state"

export const [state, setState] = createStore<State>(emptyState())

let queue: GlobalEvent[] = []
let timer: ReturnType<typeof setTimeout> | undefined
/**
 * One buffer per in-flight REST snapshot: the frames that raced it, replayed
 * after it lands. Snapshots overlap (a resync and a transcript load), so each
 * records and clears only its own buffer.
 */
const recorders = new Set<GlobalEvent[]>()

export function enqueue(frame: GlobalEvent) {
  queue.push(frame)
  for (const r of recorders) r.push(frame)
  timer ??= setTimeout(flush, 16)
}

export function flush() {
  timer = undefined
  if (!queue.length) return
  const frames = queue
  queue = []
  setState(produce((d) => frames.forEach((f) => reduce(d, f))))
}

/**
 * Apply a snapshot without losing events that raced it: frames that arrive
 * between the fetch and the apply are re-applied on top. Deltas are skipped
 * on replay (they are not idempotent, and snapshots never carry part text
 * mid-stream anyway; the next `message.part.updated` settles it).
 */
export async function withSnapshot<T>(
  fetch: () => Promise<T>,
  apply: (d: State, data: T) => void,
  /** Checked when the data arrives; false drops it (e.g. the view went away). */
  wanted: () => boolean = () => true,
) {
  const own: GlobalEvent[] = []
  recorders.add(own)
  try {
    const data = await fetch()
    if (!wanted()) return
    flush()
    setState(
      produce((d) => {
        apply(d, data)
        for (const f of own) if (f.payload.type !== "message.part.delta") reduce(d, f)
      }),
    )
  } finally {
    recorders.delete(own)
  }
}

export function setConn(conn: Partial<State["conn"]>) {
  setState("conn", conn)
}

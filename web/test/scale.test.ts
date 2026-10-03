// Fleet scale: 20 sessions with 2,000 parts each, plus a steady stream of
// deltas. The bounds are loose (CI machines vary) but catch accidental
// quadratic work in the reducer or the fleet derivations.
import { describe, expect, test } from "vitest"
import type { GlobalEvent } from "../src/api/types"
import { activity, allInterrupts, fleetState } from "../src/fleet/derive"
import { reduce } from "../src/store/reduce"
import { emptyState } from "../src/store/state"

const frame = (type: string, properties: unknown) =>
  ({ directory: "/p", payload: { id: "evt", type, properties } }) as GlobalEvent

const SESSIONS = 20
const MESSAGES = 100
const PARTS = 20
const DELTAS = 200_000

function message(sid: string, id: string) {
  return {
    id,
    sessionID: sid,
    role: "assistant",
    time: { created: 1 },
    parentID: "x",
    modelID: "m",
    providerID: "p",
    mode: "build",
    agent: "build",
    path: { cwd: "/p", root: "/p" },
    cost: 0,
    tokens: { input: 0, output: 0, reasoning: 0, cache: { read: 0, write: 0 } },
  }
}

describe("scale", () => {
  test(`${SESSIONS} sessions × ${MESSAGES * PARTS} parts, then ${DELTAS} deltas`, () => {
    const s = emptyState()
    const pad = (n: number) => String(n).padStart(6, "0")
    const t0 = performance.now()
    for (let si = 0; si < SESSIONS; si++) {
      const sid = `ses_${pad(si)}`
      s.loaded[sid] = true // as if every transcript were open: the worst case
      s.status[sid] = { type: "busy" }
      for (let mi = 0; mi < MESSAGES; mi++) {
        const mid = `msg_${pad(si)}_${pad(mi)}`
        reduce(s, frame("message.updated", { sessionID: sid, info: message(sid, mid) }))
        for (let pi = 0; pi < PARTS; pi++)
          reduce(
            s,
            frame("message.part.updated", {
              sessionID: sid,
              time: 1,
              part: {
                id: `prt_${pad(si)}_${pad(mi)}_${pad(pi)}`,
                sessionID: sid,
                messageID: mid,
                type: "text",
                text: "",
              },
            }),
          )
      }
    }
    const loaded = performance.now() - t0

    const t1 = performance.now()
    for (let i = 0; i < DELTAS; i++) {
      const si = i % SESSIONS
      reduce(
        s,
        frame("message.part.delta", {
          sessionID: `ses_${pad(si)}`,
          messageID: `msg_${pad(si)}_${pad(MESSAGES - 1)}`,
          partID: `prt_${pad(si)}_${pad(MESSAGES - 1)}_${pad(PARTS - 1)}`,
          field: "text",
          delta: "x",
        }),
      )
    }
    const streamed = performance.now() - t1

    const t2 = performance.now()
    for (let round = 0; round < 50; round++)
      for (let si = 0; si < SESSIONS; si++) {
        fleetState(s, `ses_${pad(si)}`)
        activity(s, `ses_${pad(si)}`)
      }
    allInterrupts(s)
    const derived = (performance.now() - t2) / 50

    console.log(
      `load ${loaded.toFixed(0)} ms · ${DELTAS} deltas ${streamed.toFixed(0)} ms · fleet derive ${derived.toFixed(2)} ms/round`,
    )
    expect(Object.keys(s.part)).toHaveLength(SESSIONS * MESSAGES * PARTS)
    expect((s.part[`prt_${pad(0)}_${pad(MESSAGES - 1)}_${pad(PARTS - 1)}`] as { text: string }).text).toHaveLength(
      DELTAS / SESSIONS,
    )
    expect(loaded).toBeLessThan(5_000)
    expect(streamed).toBeLessThan(2_000)
    expect(derived).toBeLessThan(20)
  })
})

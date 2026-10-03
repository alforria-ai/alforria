// Replays real /global/event recordings (bun run record) over the REST
// snapshot taken before the prompt, and checks the store ends up exactly where
// the server says it is afterwards: same messages, same parts, nothing left
// waiting. This is the contract the whole UI rests on.
import { readFileSync, readdirSync } from "node:fs"
import path from "node:path"
import { describe, expect, test } from "vitest"
import type { GlobalEvent, MessageWithParts } from "../src/api/types"
import { allInterrupts, fleetState } from "../src/fleet/derive"
import { applySnapshot, reduce } from "../src/store/reduce"
import { emptyState } from "../src/store/state"

const dir = path.join(import.meta.dirname, "fixtures")
const scenarios = readdirSync(dir)
  .filter((f) => f.endsWith(".events.jsonl"))
  .map((f) => f.replace(".events.jsonl", ""))

function load(name: string) {
  const read = (suffix: string) => readFileSync(path.join(dir, `${name}.${suffix}`), "utf8")
  const boot = JSON.parse(read("bootstrap.json"))
  const final = JSON.parse(read("final.json"))
  const events = read("events.jsonl")
    .split("\n")
    .filter(Boolean)
    .map((l) => JSON.parse(l) as GlobalEvent)
  return { boot, final, events }
}

describe.each(scenarios)("replay %s", (name) => {
  const { boot, final, events } = load(name)
  const sid: string = boot.meta.sessionID

  const replay = (onFrame?: (s: ReturnType<typeof emptyState>, f: GlobalEvent) => void) => {
    const s = emptyState()
    applySnapshot(s, boot.project, [
      {
        directory: boot.meta.directory,
        sessions: boot.session,
        status: boot.sessionStatus,
        permissions: boot.permission,
        questions: boot.question,
      },
    ])
    for (const f of events) {
      reduce(s, f)
      onFrame?.(s, f)
    }
    return s
  }

  test("ends with the server's messages and parts, for every session", () => {
    const s = replay()
    const bySession = final.messages as Record<string, MessageWithParts[]>
    expect(Object.keys(bySession)).toContain(sid)
    for (const [session, want] of Object.entries(bySession)) {
      expect(s.messages[session]).toEqual(want.map((m) => m.info.id))
      for (const m of want) {
        expect(s.message[m.info.id]).toEqual(m.info)
        expect(s.parts[m.info.id]).toEqual(m.parts.map((p) => p.id))
        for (const p of m.parts) expect(s.part[p.id]).toEqual(p)
      }
    }
  })

  test("ends with the server's session and nothing waiting", () => {
    const s = replay()
    expect(s.sessions[sid]).toEqual(final.session.find((x: { id: string }) => x.id === sid))
    expect(allInterrupts(s)).toEqual([])
    expect(fleetState(s, sid)).toBe("idle")
  })

  test("is waiting exactly while a permission is open", () => {
    const asked = events.filter((f) => f.payload.type === "permission.asked").length
    const seen: string[] = []
    replay((s, f) => {
      if (f.payload.type === "permission.asked" || f.payload.type === "permission.replied")
        seen.push(fleetState(s, sid))
    })
    expect(seen).toEqual(
      events
        .filter((f) => f.payload.type === "permission.asked" || f.payload.type === "permission.replied")
        .map((f) => (f.payload.type === "permission.asked" ? "waiting" : "working")),
    )
    expect(seen.filter((x) => x === "waiting").length).toBe(asked)
  })
})

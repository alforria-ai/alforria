import { describe, expect, test } from "vitest"
import { historyStep, loadDrafts, saveDrafts } from "../src/views/focus/drafts"

const memory = () => {
  const m = new Map<string, string>()
  return {
    m,
    getItem: (k: string) => m.get(k) ?? null,
    setItem: (k: string, v: string) => void m.set(k, v),
    removeItem: (k: string) => void m.delete(k),
  }
}

describe("persistent drafts", () => {
  test("round-trip text and choices, never attachments", () => {
    const s = memory()
    saveDrafts(
      {
        a: { text: "half a thought", model: "p/m", variant: "high", files: ["x.ts"] },
        b: { text: "  ", files: [] },
        c: { text: "", agent: "plan", files: [] },
        d: { text: "with a file", files: [], ...{ attachments: [{ name: "big.png", url: "data:…" }] } },
      },
      s,
    )
    const back = loadDrafts(s)
    expect(Object.keys(back)).toEqual(["a", "c", "d"])
    expect(back.a).toEqual({ text: "half a thought", model: "p/m", variant: "high", agent: undefined, files: ["x.ts"] })
    expect(JSON.stringify(back.d)).not.toContain("attachments")
  })

  test("nothing left clears the key", () => {
    const s = memory()
    saveDrafts({ a: { text: "x", files: [] } }, s)
    saveDrafts({ a: { text: "", files: [] } }, s)
    expect(s.m.size).toBe(0)
  })

  test("broken or blocked storage is survivable", () => {
    const s = memory()
    s.setItem("alforria.drafts", "{not json")
    expect(loadDrafts(s)).toEqual({})
    s.setItem("alforria.drafts", JSON.stringify({ a: { text: 3 }, b: { text: "ok", files: "no" } }))
    expect(loadDrafts(s)).toEqual({
      b: { text: "ok", files: [], agent: undefined, model: undefined, variant: undefined },
    })
    const blocked = {
      getItem: () => {
        throw new Error("SecurityError")
      },
      setItem: () => {
        throw new Error("QuotaExceededError")
      },
      removeItem: () => {},
    }
    expect(loadDrafts(blocked)).toEqual({})
    expect(() => saveDrafts({ a: { text: "x", files: [] } }, blocked)).not.toThrow()
    expect(loadDrafts(undefined)).toEqual({})
  })
})

describe("prompt history", () => {
  const h = ["one", "two", "three"]
  test("up walks back from the draft and stops at the oldest", () => {
    expect(historyStep(h, undefined, "up")).toEqual({ index: 2 })
    expect(historyStep(h, 2, "up")).toEqual({ index: 1 })
    expect(historyStep(h, 0, "up")).toBe("stay")
    expect(historyStep([], undefined, "up")).toBe("stay")
  })
  test("down walks forward and returns to the draft", () => {
    expect(historyStep(h, 1, "down")).toEqual({ index: 2 })
    expect(historyStep(h, 2, "down")).toEqual({ index: undefined })
    expect(historyStep(h, undefined, "down")).toBe("stay")
  })
})

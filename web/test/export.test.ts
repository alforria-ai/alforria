import { describe, expect, test } from "vitest"
import type { Message, Part, Session } from "../src/api/types"
import { emptyState } from "../src/store/state"
import { exportName, replyMarkdown, transcriptMarkdown } from "../src/views/focus/export"

function fixture() {
  const s = emptyState()
  s.sessions.ses = { id: "ses", title: "Fix the 404", directory: "/p" } as Session
  s.messages.ses = ["msg_1", "msg_2", "msg_3"]
  const t = Date.UTC(2026, 9, 12, 9, 30)
  s.message.msg_1 = { id: "msg_1", role: "user", time: { created: t } } as Message
  s.message.msg_2 = { id: "msg_2", role: "assistant", agent: "build", modelID: "m", time: { created: t } } as Message
  s.message.msg_3 = { id: "msg_3", role: "assistant", agent: "build", modelID: "m", time: { created: t } } as Message
  const parts: [string, string, Record<string, unknown>][] = [
    ["msg_1", "p1", { type: "text", text: "Why 404?" }],
    ["msg_1", "p2", { type: "text", text: "<system note>", synthetic: true }],
    ["msg_2", "p3", { type: "reasoning", text: "thinking…" }],
    ["msg_2", "p4", { type: "tool", tool: "read", state: { status: "completed", title: "src/server.ts", input: {} } }],
    ["msg_3", "p5", { type: "text", text: "The router compares exactly." }],
  ]
  for (const [mid, id, p] of parts) {
    ;(s.parts[mid] ??= []).push(id)
    s.part[id] = { id, ...p } as Part
  }
  return s
}

describe("export", () => {
  test("a reply copies as its prose, without tools or reasoning", () => {
    expect(replyMarkdown(fixture(), ["msg_2", "msg_3"])).toBe("The router compares exactly.")
  })

  test("a transcript has one heading per prompt and per reply", () => {
    expect(transcriptMarkdown(fixture(), "ses")).toBe(
      [
        "# Fix the 404",
        "",
        "/p · ses",
        "",
        "## You · 2026-10-12 09:30",
        "",
        "Why 404?",
        "",
        "## build · m · 2026-10-12 09:30",
        "",
        "- `read` src/server.ts",
        "",
        "The router compares exactly.",
        "",
      ].join("\n"),
    )
  })

  test("file names are slugs", () => {
    expect(exportName("Fix the 404 on /users!")).toBe("fix-the-404-on-users.md")
    expect(exportName("…")).toBe("transcript.md")
  })
})

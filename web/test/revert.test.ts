import { describe, expect, test } from "vitest"
import type { Message, Part, Project, Session } from "../src/api/types"
import { emptyState, type State } from "../src/store/state"
import { toolChanges } from "../src/views/focus/edits"
import { canRestoreFiles, isReverted, promptText, revertedPrompts } from "../src/views/focus/revert"

function fixture(revert?: { messageID: string }): State {
  const s = emptyState()
  s.projects.prj = { id: "prj", worktree: "/p", vcs: "git" } as Project
  s.sessions.ses = { id: "ses", projectID: "prj", directory: "/p", revert } as Session
  const msgs: [string, "user" | "assistant", string?][] = [
    ["msg_01", "user", "first"],
    ["msg_02", "assistant"],
    ["msg_03", "user", "second"],
    ["msg_04", "assistant"],
    ["msg_05", "user", "third"],
  ]
  s.messages.ses = msgs.map(([id]) => id)
  for (const [id, role, text] of msgs) {
    s.message[id] = { id, role, sessionID: "ses" } as Message
    if (!text) continue
    s.parts[id] = [`prt_${id}`, `prt_${id}_syn`]
    s.part[`prt_${id}`] = { id: `prt_${id}`, type: "text", text } as Part
    s.part[`prt_${id}_syn`] = { id: `prt_${id}_syn`, type: "text", text: "server note", synthetic: true } as Part
  }
  return s
}

describe("revert display", () => {
  test("nothing is rolled back without session.revert", () => {
    const s = fixture()
    expect(isReverted(s, "ses", "msg_05")).toBe(false)
    expect(revertedPrompts(s, "ses")).toEqual([])
  })

  test("turns at and after revert.messageID are rolled back", () => {
    const s = fixture({ messageID: "msg_03" })
    expect(["msg_01", "msg_02", "msg_03", "msg_04", "msg_05"].map((id) => isReverted(s, "ses", id))).toEqual([
      false,
      false,
      true,
      true,
      true,
    ])
    expect(revertedPrompts(s, "ses")).toEqual([
      { id: "msg_03", text: "second" },
      { id: "msg_05", text: "third" },
    ])
  })

  test("the restored prompt skips synthetic text", () => {
    expect(promptText(fixture(), "msg_01")).toBe("first")
  })

  test("only git projects can restore files", () => {
    const s = fixture()
    expect(canRestoreFiles(s, "ses")).toBe(true)
    s.projects.prj = { ...s.projects.prj!, vcs: undefined }
    expect(canRestoreFiles(s, "ses")).toBe(false)
  })
})

describe("edit fallback", () => {
  test("apply_patch counts every file in metadata.files", () => {
    const changes = toolChanges(
      "apply_patch",
      { patchText: "…" },
      {
        filepath: "a.ts, b.ts",
        diff: "combined",
        files: [
          { filePath: "/p/a.ts", relativePath: "a.ts", type: "update", patch: "@@ -1 +1 @@\n-x\n+y" },
          { filePath: "/p/b.ts", relativePath: "c.ts", movePath: "/p/c.ts", type: "move", patch: "" },
        ],
      },
    )
    expect(changes).toEqual([
      { file: "/p/a.ts", patch: "@@ -1 +1 @@\n-x\n+y" },
      { file: "/p/c.ts", patch: "" },
    ])
  })

  test("write without a diff becomes an all-added patch", () => {
    expect(toolChanges("write", { filePath: "/p/n.txt", content: "a\nb\n" }, {})).toEqual([
      { file: "/p/n.txt", patch: "@@ -0,0 +1,2 @@\n+a\n+b" },
    ])
  })
})

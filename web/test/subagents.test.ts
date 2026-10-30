import { describe, expect, test } from "vitest"
import { failureLine, noteOf, shortTitle, toolErrorText } from "../src/views/focus/subagent-notes"

describe("subagent notes", () => {
  test("recognizes an automatic resume", () => {
    expect(
      noteOf(
        "<auto_resume>\nYour previous attempt stopped with an error: boom\nmore\n\nContinue the task.\n</auto_resume>",
      ),
    ).toEqual({ kind: "resume", error: "boom" })
  })

  test("recognizes a result delivered to the parent", () => {
    expect(
      noteOf(
        '<task id="ses_1" state="error">\n<summary>The user continued this subagent directly: Research (@general subagent)</summary>\n<task_error>\nboom\n</task_error>\n</task>',
      ),
    ).toEqual({
      kind: "update",
      session: "ses_1",
      state: "error",
      summary: "You continued Research. The main agent has its result.",
    })
  })

  test("leaves other synthetic text alone", () => {
    expect(noteOf("The following tool was executed by the user")).toBeUndefined()
  })
})

describe("subagent failures", () => {
  const stored = JSON.stringify({
    name: "Error",
    data: { message: "Subagent failed (task_id: ses_1): boom\n\nIts session is intact." },
  })

  test("reads a stored error as its message", () => {
    expect(toolErrorText(stored)).toContain("Its session is intact.")
    expect(toolErrorText("plain words")).toBe("plain words")
  })

  test("keeps only why it failed", () => {
    expect(failureLine(stored)).toBe("boom")
    expect(failureLine("plain words")).toBe("plain words")
  })

  test("titles lose the agent suffix", () => {
    expect(shortTitle("Research (@general subagent)")).toBe("Research")
    expect(shortTitle("Plain title")).toBe("Plain title")
  })
})

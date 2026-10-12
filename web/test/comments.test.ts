import { describe, expect, test } from "vitest"
import { commentsAt, commentsText, type LineComment } from "../src/views/focus/comments"

const c = (file: string, line: number, text: string, extra: Partial<LineComment> = {}): LineComment => ({
  id: `${file}${line}`,
  file,
  line,
  side: "new",
  code: "",
  text,
  ...extra,
})

describe("review comments", () => {
  test("quote file, line and code, in file then line order", () => {
    expect(
      commentsText([
        c("src/b.ts", 3, "why?"),
        c("src/a.ts", 12, "rename `x`\nto count", { code: "  const `x` = 1" }),
        c("src/a.ts", 4, "keep this", { side: "old", code: "return y" }),
      ]),
    ).toBe(
      [
        "Review comments on specific lines:",
        "- src/a.ts:4 (removed line) `return y`",
        "  keep this",
        "- src/a.ts:12 `const 'x' = 1`",
        "  rename `x`",
        "  to count",
        "- src/b.ts:3",
        "  why?",
      ].join("\n"),
    )
  })

  test("nothing to say without comments", () => {
    expect(commentsText([])).toBe("")
  })

  test("comments are found per file, line and side", () => {
    const list = [c("a", 1, "x"), c("a", 1, "y", { side: "old" }), c("b", 1, "z")]
    expect(commentsAt(list, "a", 1, "new").map((x) => x.text)).toEqual(["x"])
    expect(commentsAt(undefined, "a", 1, "new")).toEqual([])
  })
})

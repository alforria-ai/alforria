import { describe, expect, test } from "vitest"
import type { Message } from "../src/api/types"
import { emptyState } from "../src/store/state"
import {
  attachmentKind,
  commandBody,
  fileUrl,
  lastUserModel,
  looksBinary,
  MENTION,
  promptBody,
  type Draft,
} from "../src/views/focus/prompt"

const draft = (extra: Partial<Draft> = {}): Draft => ({
  text: "look at @src/a b.ts",
  files: ["src/a b.ts", "gone.ts"],
  attachments: [],
  directory: "/home/me/my proj/",
  agent: "build",
  model: "mock/mock-model",
  ...extra,
})

describe("file mentions", () => {
  test("file:// URLs encode each segment", () => {
    expect(fileUrl("/home/me/my proj", "src/a b#1?.ts")).toBe("file:///home/me/my%20proj/src/a%20b%231%3F.ts")
    expect(fileUrl("/r/", "é.md")).toBe("file:///r/%C3%A9.md")
  })

  test("@ completion takes any non-space path", () => {
    expect(MENTION.exec("see @src/[id]/page+x.tsx")?.[2]).toBe("src/[id]/page+x.tsx")
    expect(MENTION.exec("mail me@x")).toBeNull()
    expect(MENTION.exec("@")?.[2]).toBe("")
  })
})

describe("prompt body", () => {
  test("text, mentioned files, attachments, model and variant", () => {
    const body = promptBody(
      draft({ variant: "high", attachments: [{ name: "x.png", mime: "image/png", url: "data:image/png;base64,AA" }] }),
    )
    expect(body).toEqual({
      parts: [
        { type: "text", text: "look at @src/a b.ts" },
        { type: "file", mime: "text/plain", filename: "src/a b.ts", url: "file:///home/me/my%20proj/src/a%20b.ts" },
        { type: "file", mime: "image/png", filename: "x.png", url: "data:image/png;base64,AA" },
      ],
      agent: "build",
      model: { providerID: "mock", modelID: "mock-model" },
      variant: "high",
    })
  })

  test("no variant key without a variant", () => {
    expect("variant" in promptBody(draft())).toBe(false)
  })
})

describe("command body", () => {
  test("file parts and the variant go with a slash command", () => {
    expect(commandBody(draft({ text: "/review @src/a b.ts", variant: "low" }), "review", "@src/a b.ts")).toEqual({
      command: "review",
      arguments: "@src/a b.ts",
      agent: "build",
      model: "mock/mock-model",
      variant: "low",
      parts: [
        { type: "file", mime: "text/plain", filename: "src/a b.ts", url: "file:///home/me/my%20proj/src/a%20b.ts" },
      ],
    })
  })

  test("no parts key without files", () => {
    expect(commandBody(draft({ text: "/init", files: [] }), "init", "")).toEqual({
      command: "init",
      arguments: "",
      agent: "build",
      model: "mock/mock-model",
    })
  })
})

describe("the session's model", () => {
  test("comes from the last user message, variant included", () => {
    const s = emptyState()
    s.messages.ses = ["msg_1", "msg_2", "msg_3"]
    s.message.msg_1 = { id: "msg_1", role: "user", model: { providerID: "a", modelID: "old" } } as Message
    s.message.msg_2 = {
      id: "msg_2",
      role: "user",
      model: { providerID: "a", modelID: "m", variant: "high" },
    } as Message
    s.message.msg_3 = { id: "msg_3", role: "assistant" } as Message
    expect(lastUserModel(s, "ses")).toEqual({ providerID: "a", modelID: "m", variant: "high" })
  })
})

describe("attachments", () => {
  test("text, images and PDFs pass; archives and binaries don't", () => {
    expect(attachmentKind("a.png", "image/png")).toBe("image")
    expect(attachmentKind("a.pdf", "application/pdf")).toBe("pdf")
    expect(attachmentKind("a.ts", "video/mp2t")).toBe("text")
    expect(attachmentKind("notes", "text/plain")).toBe("text")
    expect(attachmentKind("Dockerfile", "")).toBe("text")
    expect(attachmentKind("a.zip", "application/zip")).toBeUndefined()
    expect(attachmentKind("a.mp4", "video/mp4")).toBeUndefined()
    expect(attachmentKind("a.tiff", "image/tiff")).toBeUndefined()
    expect(attachmentKind("blob", "")).toBe("maybe-text")
  })

  test("a NUL byte marks a binary", () => {
    expect(looksBinary(new TextEncoder().encode("plain text"))).toBe(false)
    expect(looksBinary(new Uint8Array([0x50, 0x4b, 0x03, 0x04, 0x00]))).toBe(true)
  })
})

import { describe, expect, test } from "vitest"
import type { AssistantMessage, PermissionRequest, QuestionRequest } from "../src/api/types"
import { allInterrupts, contextUse, fleetState } from "../src/fleet/derive"
import { emptyState } from "../src/store/state"
import { finalAnswers } from "../src/views/queue/drafts"

const permission = (id: string): PermissionRequest => ({
  id,
  sessionID: "ses",
  permission: "bash",
  patterns: ["ls"],
  metadata: {},
  always: [],
})

const question = (id: string, multiple = false): QuestionRequest =>
  ({
    id,
    sessionID: "ses",
    questions: [
      {
        question: "Which?",
        header: "Pick",
        multiple,
        options: [
          { label: "A", description: "" },
          { label: "B", description: "" },
        ],
      },
    ],
  }) as QuestionRequest

const assistant = (id: string, output: number, input = 100): AssistantMessage =>
  ({
    id,
    sessionID: "ses",
    role: "assistant",
    providerID: "p",
    modelID: "m",
    cost: 0,
    time: { created: 1 },
    tokens: { input, output, reasoning: 0, cache: { read: 0, write: 0 } },
  }) as AssistantMessage

describe("interrupts", () => {
  test("a mixed permission/question queue is in age order", () => {
    const s = emptyState()
    // Ids sort by their body: the question was asked between the permissions.
    s.permissions = { per_01B: permission("per_01B"), per_01D: permission("per_01D") }
    s.questions = { que_01C: question("que_01C"), que_01A: question("que_01A") }
    expect(allInterrupts(s).map((i) => i.id)).toEqual(["que_01A", "per_01B", "que_01C", "per_01D"])
  })
})

describe("fleetState", () => {
  test("Stop is not a fault", () => {
    const s = emptyState()
    s.errors.ses = { name: "MessageAbortedError", message: "aborted" }
    expect(fleetState(s, "ses")).toBe("idle")
    s.errors.ses = { name: "ProviderAuthError", message: "no key" }
    expect(fleetState(s, "ses")).toBe("fault")
  })
})

describe("contextUse", () => {
  test("reads the last assistant turn that produced output", () => {
    const s = emptyState()
    s.models["p/m"] = { name: "m", context: 1000 }
    s.messages.ses = ["msg_1", "msg_2"]
    s.message.msg_1 = assistant("msg_1", 100, 300)
    // A turn still streaming (or stopped before any output) reports zeros.
    s.message.msg_2 = assistant("msg_2", 0, 0)
    expect(contextUse(s, "ses")).toEqual({ tokens: 400, fraction: 0.4 })
  })
})

describe("question drafts", () => {
  test("typed text replaces a single choice: one answer is sent", () => {
    const q = question("que_1")
    expect(finalAnswers(q, { answers: [["A"]], custom: ["  my own  "] })).toEqual([["my own"]])
    expect(finalAnswers(q, { answers: [["A"]], custom: [""] })).toEqual([["A"]])
  })
  test("typed text adds to a multiple choice", () => {
    const q = question("que_1", true)
    expect(finalAnswers(q, { answers: [["A", "B"]], custom: ["C"] })).toEqual([["A", "B", "C"]])
  })
})

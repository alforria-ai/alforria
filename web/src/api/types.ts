// Friendly names over the generated OpenAPI types (src/api/schema.d.ts, from
// fixtures/openapi/openapi.json). Regenerate with `bun run gen:api`.
import type { components } from "./schema"

type Schemas = components["schemas"]

// openapi-typescript renders open objects (`metadata`, tool `input`, JSON
// schemas) as Record<string, never>; widen exactly those slots, everywhere,
// so reads type-check and every exported type agrees with every other.
type IsClosedRecord<T> = T extends Record<string, never> ? (string extends keyof T ? true : false) : false
type Open<T> =
  IsClosedRecord<T> extends true
    ? Record<string, unknown>
    : T extends (infer U)[]
      ? Open<U>[]
      : T extends object
        ? { [K in keyof T]: Open<T[K]> }
        : T

export type Project = Open<Schemas["Project"]>
export type Session = Open<Schemas["Session"]>
export type SessionStatus = Open<Schemas["SessionStatus"]>
export type Message = Open<Schemas["Message"]>
export type UserMessage = Open<Schemas["UserMessage"]>
export type AssistantMessage = Open<Schemas["AssistantMessage"]>
export type Part = Open<Schemas["Part"]>
export type TextPart = Open<Schemas["TextPart"]>
export type ReasoningPart = Open<Schemas["ReasoningPart"]>
export type ToolPart = Open<Schemas["ToolPart"]>
export type ToolState = Open<Schemas["ToolState"]>
export type StepFinishPart = Open<Schemas["StepFinishPart"]>
export type PermissionRequest = Open<Schemas["PermissionRequest"]>
export type QuestionRequest = Open<Schemas["QuestionRequest"]>
export type QuestionInfo = Open<Schemas["QuestionInfo"]>
export type Todo = Open<Schemas["Todo"]>
export type FileDiff = Open<Schemas["FileDiff"]>
export type Agent = Open<Schemas["Agent"]>
export type Command = Open<Schemas["Command"]>

export type GlobalEvent = Open<Schemas["GlobalEvent"]>
export type EventPayload = GlobalEvent["payload"]
export type EventType = EventPayload["type"]
/** The payload variant for one event type, e.g. `Payload<"session.status">`. */
export type Payload<T extends EventType> = Extract<EventPayload, { type: T }>

export type PermissionReply = "once" | "always" | "reject"

/** A message with its parts, as `GET /session/{id}/message` returns it. */
export interface MessageWithParts {
  info: Message
  parts: Part[]
}

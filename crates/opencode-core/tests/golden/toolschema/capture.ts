/**
 * Golden-capture script for the tool parameter JSON schemas (spec §2.3).
 * Run with `bun run capture.ts` from this directory after `bun add
 * effect@4.0.0-beta.83` (see /tmp/opencode-src pinned commit
 * 88c6c7abc7f320b6aabed2634ac0b2d6e6ecea67). It executes the real
 * `ToolJsonSchema.fromSchema` pipeline (tool/json-schema.ts, copied verbatim)
 * against the read/glob/grep (M4.2) and todowrite/question/skill/invalid
 * (M4.6) `Schema.Struct` parameters and rewrites the corresponding .json
 * files in this directory.
 */
import { JsonSchema, Schema } from "effect"

// ---- from json-schema.ts (copied verbatim) ----
type JsonObject = Record<string, unknown>
const isRecord = (value: unknown): value is JsonObject =>
  typeof value === "object" && value !== null && !Array.isArray(value)
const isJsonSchema = (value: unknown): value is Record<string, unknown> =>
  typeof value === "boolean" || isRecord(value)
const isNonFiniteNumber = (value: unknown) =>
  value === "NaN" || value === "Infinity" || value === "-Infinity"

function isEmptyStructUnion(items: unknown[]) {
  return (
    items.length === 2 &&
    items.some((item) => isRecord(item) && item.type === "object" && item.properties === undefined) &&
    items.some((item) => isRecord(item) && item.type === "array" && item.items === undefined)
  )
}

function canFlattenAllOf(allOf: JsonObject[], parent: JsonObject) {
  const keys = new Set(Object.keys(parent).filter((key) => key !== "allOf"))
  return allOf.every((item) =>
    Object.keys(item).every((key) => {
      if (keys.has(key)) return false
      keys.add(key)
      return true
    }),
  )
}

function normalize(value: unknown, options: { stripNull?: boolean } = {}): unknown {
  if (Array.isArray(value)) return value.map((item) => normalize(item))
  if (!isRecord(value)) return value

  const required = Array.isArray(value.required)
    ? new Set(value.required.filter((item) => typeof item === "string"))
    : undefined
  const schema = Object.fromEntries(
    Object.entries(value).map(([key, item]) => {
      if (key === "properties" && isRecord(item)) {
        return [
          key,
          Object.fromEntries(
            Object.entries(item).map(([name, property]) => [
              name,
              normalize(property, { stripNull: !required?.has(name) }),
            ]),
          ),
        ]
      }
      return [key, normalize(item)]
    }),
  )

  if (schema.additionalProperties === true) delete schema.additionalProperties

  if (options.stripNull && Array.isArray(schema.anyOf)) {
    const withoutNull = schema.anyOf.filter((item) => !isRecord(item) || item.type !== "null")
    if (withoutNull.length !== schema.anyOf.length) return normalize({ ...schema, anyOf: withoutNull })
  }

  if (Array.isArray(schema.anyOf)) {
    const withoutNull = schema.anyOf
    const number = withoutNull.find((item) => isRecord(item) && item.type === "number")
    const nonFinite = withoutNull.filter(
      (item) => isRecord(item) && Array.isArray(item.enum) && item.enum.every((entry) => isNonFiniteNumber(entry)),
    )
    if (number && nonFinite.length === withoutNull.length - 1) {
      const { anyOf: _, ...rest } = schema
      return normalize({ ...number, ...rest })
    }

    if (isEmptyStructUnion(withoutNull)) {
      const { anyOf: _, ...rest } = schema
      return normalize({ type: "object", properties: {}, ...rest })
    }

    if (withoutNull.length === 1 && isRecord(withoutNull[0])) {
      const { anyOf: _, ...rest } = schema
      return normalize({ ...withoutNull[0], ...rest })
    }
  }

  if (Array.isArray(schema.allOf) && schema.allOf.every(isRecord) && canFlattenAllOf(schema.allOf, schema)) {
    const { allOf, ...rest } = schema
    return normalize({ ...Object.assign({}, ...allOf), ...rest })
  }

  if (schema.type === "integer" && schema.maximum === undefined) {
    return { minimum: Number.MIN_SAFE_INTEGER, ...schema, maximum: Number.MAX_SAFE_INTEGER }
  }

  return schema
}

function inlineLocalReferences(value: unknown, definitions?: JsonObject, seen = new Set<string>()): unknown {
  if (Array.isArray(value)) return value.map((item) => inlineLocalReferences(item, definitions, seen))
  if (!isRecord(value)) return value

  const localDefinitions = definitions ?? (isRecord(value.$defs) ? value.$defs : undefined)
  if (typeof value.$ref === "string" && localDefinitions) {
    const name = value.$ref.match(/^#\/\$defs\/(.+)$/)?.[1] ?? value.$ref.match(/^#\/definitions\/(.+)$/)?.[1]
    if (name && !seen.has(name)) {
      const target = localDefinitions[name]
      if (target) {
        const { $ref: _, ...rest } = value
        return inlineLocalReferences(
          { ...(isRecord(target) ? target : {}), ...rest },
          localDefinitions,
          new Set(seen).add(name),
        )
      }
    }
  }

  return Object.fromEntries(
    Object.entries(value).map(([key, item]) => [key, inlineLocalReferences(item, localDefinitions, seen)]),
  )
}

function dropDefinitionsIfResolved(value: unknown): unknown {
  if (!isRecord(value) || hasLocalReference(value)) return value
  const { $defs: _, definitions: __, ...rest } = value
  return rest
}

function hasLocalReference(value: unknown): boolean {
  if (Array.isArray(value)) return value.some(hasLocalReference)
  if (!isRecord(value)) return false
  if (
    typeof value.$ref === "string" &&
    (value.$ref.startsWith("#/$defs/") || value.$ref.startsWith("#/definitions/"))
  ) {
    return true
  }
  return Object.values(value).some(hasLocalReference)
}

function fromSchema(schema: Schema.Top): Record<string, unknown> {
  const document = Schema.toJsonSchemaDocument(schema, { additionalProperties: true })
  const result = normalize({
    $schema: JsonSchema.META_SCHEMA_URI_DRAFT_2020_12,
    ...document.schema,
    ...(Object.keys(document.definitions).length > 0 ? { $defs: document.definitions } : {}),
  })
  const inlined = dropDefinitionsIfResolved(inlineLocalReferences(result))
  if (!isJsonSchema(inlined)) throw new Error("tool JSON Schema helper produced a non-schema value")
  return inlined as Record<string, unknown>
}

// ---- Parameters (copied from the tool files) ----
const NonNegativeInt = Schema.Int.check(Schema.isGreaterThanOrEqualTo(0))

const ReadParameters = Schema.Struct({
  filePath: Schema.String.annotate({ description: "The absolute path to the file or directory to read" }),
  offset: Schema.optional(NonNegativeInt).annotate({
    description: "The line number to start reading from (1-indexed)",
  }),
  limit: Schema.optional(NonNegativeInt).annotate({
    description: "The maximum number of lines to read (defaults to 2000)",
  }),
})

const GlobParameters = Schema.Struct({
  pattern: Schema.String.annotate({ description: "The glob pattern to match files against" }),
  path: Schema.optional(Schema.String).annotate({
    description: `The directory to search in. If not specified, the current working directory will be used. IMPORTANT: Omit this field to use the default directory. DO NOT enter "undefined" or "null" - simply omit it for the default behavior. Must be a valid directory path if provided.`,
  }),
})

const GrepParameters = Schema.Struct({
  pattern: Schema.String.annotate({ description: "The regex pattern to search for in file contents" }),
  path: Schema.optional(Schema.String).annotate({
    description: "The directory to search in. Defaults to the current working directory.",
  }),
  include: Schema.optional(Schema.String).annotate({
    description: 'File pattern to include in the search (e.g., "*.js", "*.{ts,tsx}")',
  }),
})

// ---- M4.6 Parameters (copied from the tool files / schema-src) ----

// schema-src/session-todo.ts — SessionTodo.Info
const TodoInfo = Schema.Struct({
  content: Schema.String.annotate({ description: "Brief description of the task" }),
  status: Schema.String.annotate({
    description: "Current status of the task: pending, in_progress, completed, cancelled",
  }),
  priority: Schema.String.annotate({
    description: "Priority level of the task: high, medium, low",
  }),
}).annotate({ identifier: "Todo" })

// tool/todo.ts
const TodoWriteParameters = Schema.Struct({
  todos: Schema.mutable(Schema.Array(TodoInfo)).annotate({ description: "The updated todo list" }),
})

// schema-src/v1/question.ts — QuestionV1.Option / Prompt
const QuestionOption = Schema.Struct({
  label: Schema.String.annotate({ description: "Display text (1-5 words, concise)" }),
  description: Schema.String.annotate({ description: "Explanation of choice" }),
}).annotate({ identifier: "QuestionOption" })

const QuestionPromptBase = {
  question: Schema.String.annotate({ description: "Complete question" }),
  header: Schema.String.annotate({ description: "Very short label (max 30 chars)" }),
  options: Schema.Array(QuestionOption).annotate({ description: "Available choices" }),
  multiple: Schema.optional(Schema.Boolean).annotate({ description: "Allow selecting multiple choices" }),
}
const QuestionPrompt = Schema.Struct(QuestionPromptBase).annotate({ identifier: "QuestionPrompt" })

// tool/question.ts
const QuestionParameters = Schema.Struct({
  questions: Schema.mutable(Schema.Array(QuestionPrompt)).annotate({ description: "Questions to ask" }),
})

// tool/skill.ts
const SkillParameters = Schema.Struct({
  name: Schema.String.annotate({ description: "The name of the skill from available_skills" }),
})

// tool/invalid.ts
const InvalidParameters = Schema.Struct({
  tool: Schema.String,
  error: Schema.String,
})

// tool/webfetch.ts
const WebFetchParameters = Schema.Struct({
  url: Schema.String.annotate({ description: "The URL to fetch content from" }),
  format: Schema.Literals(["text", "markdown", "html"]).annotate({
    description: "The format to return the content in (text, markdown, or html). Defaults to markdown.",
    default: "markdown",
  }).pipe(Schema.withDecodingDefault(Effect.succeed("markdown" as const))),
  timeout: Schema.optional(Schema.Number).annotate({ description: "Optional timeout in seconds (max 120)" }),
})

// tool/websearch.ts
const WebSearchParameters = Schema.Struct({
  query: Schema.String.annotate({ description: "Websearch query" }),
  numResults: Schema.optional(Schema.Number).annotate({
    description: "Number of search results to return (default: 8)",
  }),
  livecrawl: Schema.optional(Schema.Literals(["fallback", "preferred"])).annotate({
    description:
      "Live crawl mode - 'fallback': use live crawling as backup if cached content unavailable, 'preferred': prioritize live crawling (default: 'fallback')",
  }),
  type: Schema.optional(Schema.Literals(["auto", "fast", "deep"])).annotate({
    description: "Search type - 'auto': balanced search (default), 'fast': quick results, 'deep': comprehensive search",
  }),
  contextMaxCharacters: Schema.optional(Schema.Number).annotate({
    description: "Maximum characters for context string optimized for LLMs (default: 10000)",
  }),
})

await Bun.write("out-read.json", JSON.stringify(fromSchema(ReadParameters as any), null, 2) + "\n")
await Bun.write("out-glob.json", JSON.stringify(fromSchema(GlobParameters as any), null, 2) + "\n")
await Bun.write("out-grep.json", JSON.stringify(fromSchema(GrepParameters as any), null, 2) + "\n")
await Bun.write("out-todowrite.json", JSON.stringify(fromSchema(TodoWriteParameters as any), null, 2) + "\n")
await Bun.write("out-question.json", JSON.stringify(fromSchema(QuestionParameters as any), null, 2) + "\n")
await Bun.write("out-skill.json", JSON.stringify(fromSchema(SkillParameters as any), null, 2) + "\n")
await Bun.write("out-invalid.json", JSON.stringify(fromSchema(InvalidParameters as any), null, 2) + "\n")
await Bun.write("out-webfetch.json", JSON.stringify(fromSchema(WebFetchParameters as any), null, 2) + "\n")
await Bun.write("out-websearch.json", JSON.stringify(fromSchema(WebSearchParameters as any), null, 2) + "\n")

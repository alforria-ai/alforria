// A scripted OpenAI-compatible chat backend for `alforria serve`.
//
// Turns use the e2e transcript format (crates/alforria/tests/e2e_agent/
// fixtures/*.json) and are lowered to SSE chunks exactly as transcript.rs
// does, so the engine runs its real openai-chat decode path. Requests
// without `tools` are the engine's title/summary forks; they get a canned
// title and never consume script turns.

import { readFileSync } from "node:fs"
import { join } from "node:path"
import { REPO } from "./alforria"

export type Frame =
  | { text: string }
  | { tool_call: { id: string; name: string; arguments: unknown } }
  | { finish: { reason: string; usage?: { input: number; output: number } } }
  | { sleep_ms: number }

// `error` turns answer with a plain HTTP error instead of a stream.
export type Turn = { frames: Frame[] } | { error: { status: number; message: string } }

export type ChatRequest = {
  model?: string
  tools?: unknown[]
  messages?: { role: string; content?: unknown }[]
}

// Picks the turn for one agent-loop request.
export type Picker = (request: ChatRequest) => Turn

const FIXTURES = join(REPO, "crates/alforria/tests/e2e_agent/fixtures")

export function loadFixture(name: string): Turn[] {
  return JSON.parse(readFileSync(join(FIXTURES, `${name}.json`), "utf8")).turns
}

export function textTurn(text: string, usage = { input: 40, output: 8 }): Turn {
  return { frames: [{ text }, { finish: { reason: "stop", usage } }] }
}

// Served once a script is exhausted or no script matches, so the backend
// never runs dry (a human may keep prompting the dev fleet).
export const FALLBACK = textTurn("Done. (scripted backend: no more turns for this conversation)")

const sse = (payload: unknown) => `data: ${JSON.stringify(payload)}\n\n`
const delta = (value: unknown) => ({ choices: [{ index: 0, delta: value }] })

// transcript.rs `split_arguments`: halves exercise incremental tool-call
// argument assembly.
function splitArguments(args: string): string[] {
  if (!args) return []
  const mid = Math.floor(args.length / 2)
  return [args.slice(0, mid), args.slice(mid)]
}

// transcript.rs `turn_chunks`: frames batch into one chunk until a
// `sleep_ms`, which stalls the chunk after it. Unlike transcript.rs, every
// sleep stalls where it sits (it only honours one), so a turn can drip.
export function lower(frames: Frame[]): { body: string; delayMs: number }[] {
  const out: { body: string; delayMs: number }[] = []
  let current = ""
  let delay = 0
  let toolIndex = 0
  for (const frame of frames) {
    if ("sleep_ms" in frame) {
      if (current) out.push({ body: current, delayMs: delay })
      current = ""
      delay = frame.sleep_ms
      continue
    }
    if ("text" in frame) current += sse(delta({ content: frame.text }))
    if ("tool_call" in frame) {
      const call = frame.tool_call
      current += sse(
        delta({
          tool_calls: [
            { index: toolIndex, id: call.id, type: "function", function: { name: call.name, arguments: "" } },
          ],
        }),
      )
      for (const piece of splitArguments(JSON.stringify(call.arguments))) {
        current += sse(delta({ tool_calls: [{ index: toolIndex, function: { arguments: piece } }] }))
      }
      toolIndex++
    }
    if ("finish" in frame) {
      const payload: Record<string, unknown> = {
        choices: [{ index: 0, delta: {}, finish_reason: frame.finish.reason }],
      }
      const usage = frame.finish.usage
      if (usage) payload.usage = { prompt_tokens: usage.input, completion_tokens: usage.output }
      current += sse(payload)
    }
  }
  out.push({ body: current + "data: [DONE]\n\n", delayMs: delay })
  return out
}

// The plain text of every message with the given role, in order.
export function texts(request: ChatRequest, role: string): string[] {
  return (request.messages ?? [])
    .filter((message) => message.role === role)
    .map((message) => {
      const content = message.content
      if (typeof content === "string") return content
      if (Array.isArray(content))
        return content.map((part) => (typeof part?.text === "string" ? part.text : "")).join("")
      return ""
    })
}

// Assistant messages after the last user message: the step index of the
// current agent loop (each tool-call step adds one assistant message).
export function stepIndex(request: ChatRequest): number {
  const messages = request.messages ?? []
  let steps = 0
  for (let i = messages.length - 1; i >= 0 && messages[i]!.role !== "user"; i--) {
    if (messages[i]!.role === "assistant") steps++
  }
  return steps
}

// FIFO over a fixture's turns, like the e2e mock: right for a single
// conversation (subagent children consume turns in arrival order).
export function queue(turns: Turn[]): Picker {
  const pending = [...turns]
  return () => pending.shift() ?? FALLBACK
}

// One scripted conversation: the prompt that selects it, the title the
// title fork answers, and one turn per agent-loop step.
export type Script = { prompt: string; title: string; turns: Turn[] }

// Stateless routing for many concurrent sessions: the last user message
// selects the script (a subagent child's user message is its task prompt)
// and the step index selects the turn. A follow-up a human types matches
// nothing and gets FALLBACK.
export function keyed(scripts: Script[]) {
  const find = (text: string | undefined) => (text ? scripts.find((script) => text.includes(script.prompt)) : undefined)
  return {
    pick: (request: ChatRequest): Turn => {
      const script = find(texts(request, "user").at(-1))
      return script?.turns[stepIndex(request)] ?? FALLBACK
    },
    title: (request: ChatRequest) => find(texts(request, "user").join("\n"))?.title ?? "Scripted session",
  }
}

export type Backend = {
  url: string
  requests: ChatRequest[]
  stop(): void
}

export function startBackend(opts: {
  pick: Picker
  title?: (request: ChatRequest) => string
  port?: number
  hostname?: string
}): Backend {
  const requests: ChatRequest[] = []
  const server = Bun.serve({
    port: opts.port ?? 0,
    hostname: opts.hostname ?? "127.0.0.1",
    // Slow turns stall for seconds between chunks; never time them out.
    idleTimeout: 0,
    async fetch(req) {
      const url = new URL(req.url)
      if (req.method !== "POST" || !url.pathname.endsWith("/chat/completions")) {
        return new Response("not found", { status: 404 })
      }
      const request = (await req.json()) as ChatRequest
      requests.push(request)
      const turn = request.tools ? opts.pick(request) : textTurn(opts.title?.(request) ?? "Scripted session")
      if ("error" in turn) {
        return Response.json({ error: { message: turn.error.message } }, { status: turn.error.status })
      }
      const chunks = lower(turn.frames)
      const encoder = new TextEncoder()
      const stream = new ReadableStream({
        async pull(controller) {
          const chunk = chunks.shift()
          if (!chunk) return controller.close()
          if (chunk.delayMs) await Bun.sleep(chunk.delayMs)
          // The engine may have aborted (and cancelled us) mid-stall.
          try {
            controller.enqueue(encoder.encode(chunk.body))
          } catch {}
        },
        cancel() {
          chunks.length = 0
        },
      })
      return new Response(stream, { headers: { "content-type": "text/event-stream", "cache-control": "no-cache" } })
    },
  })
  return {
    url: `http://127.0.0.1:${server.port}/v1`,
    requests,
    stop: () => server.stop(true),
  }
}

// Renderers for message parts. Tool calls are datasheet "spec rows": a mark,
// the tool name in condensed caps, its target in mono, and a preview body
// chosen per tool (output, diff, todo list, subagent link).
import { createEffect, createMemo, createSignal, For, Match, onCleanup, Show, Switch } from "solid-js"
import { api } from "../../api/client"
import type { Part, ToolPart, WorkflowRun } from "../../api/types"
import { activity, fleetState, lastError, toolTarget, type Interrupt } from "../../fleet/derive"
import { state } from "../../store/store"
import { Diff, diffStats, parseUnifiedDiff } from "../../ui/diff"
import { Markdown } from "../../ui/markdown"
import { Icon } from "../../ui/icons"
import { Slip } from "../queue/slip"
import { failureLine, shortTitle, toolErrorText } from "./subagent-notes"

type State = {
  status: string
  input?: Record<string, unknown>
  output?: string
  error?: string
  title?: string
  metadata?: Record<string, unknown>
  time?: { start: number; end?: number }
}

function seconds(start?: number, end?: number) {
  if (!start || !end) return ""
  const s = (end - start) / 1000
  return s < 10 ? `${s.toFixed(1)}s` : s < 60 ? `${Math.round(s)}s` : `${Math.floor(s / 60)}m ${Math.round(s % 60)}s`
}

const OUTPUT_LINES = 400

function tail(text: string, lines = OUTPUT_LINES) {
  const all = text.replace(/\n$/, "").split("\n")
  return all.length > lines
    ? { text: all.slice(-lines).join("\n"), hidden: all.length - lines }
    : { text: all.join("\n"), hidden: 0 }
}

export function PartView(props: {
  part: Part
  streaming: boolean
  interrupt?: Interrupt
  onOpenSession: (id: string, beside: boolean) => void
}) {
  return (
    <Switch>
      <Match when={props.part.type === "text" && !(props.part as { ignored?: boolean }).ignored && props.part}>
        {(p) => <Markdown text={(p() as { text: string }).text} streaming={props.streaming} />}
      </Match>
      <Match when={props.part.type === "reasoning" && props.part}>
        {(p) => <Reasoning part={p() as Extract<Part, { type: "reasoning" }>} />}
      </Match>
      <Match when={props.part.type === "tool" && props.part}>
        {(p) => <ToolView part={p() as ToolPart} interrupt={props.interrupt} onOpenSession={props.onOpenSession} />}
      </Match>
      <Match when={props.part.type === "file" && props.part}>
        {(p) => {
          const f = p() as { filename?: string; url: string; mime: string }
          return (
            <div class="attach-chip">
              <Icon name="file" />
              <span class="mono">{f.filename ?? f.url.replace(/^file:\/\//, "")}</span>
              <span class="dim">{f.mime}</span>
            </div>
          )
        }}
      </Match>
      <Match when={props.part.type === "patch" && props.part}>
        {(p) => {
          const files = (p() as { files: string[] }).files
          return (
            <div class="verdict-row">
              <b>Changed</b>
              <span>
                {files.length} file{files.length === 1 ? "" : "s"}:{" "}
                {files
                  .slice(0, 3)
                  .map((f) => f.split("/").pop())
                  .join(", ")}
                {files.length > 3 ? ` +${files.length - 3}` : ""}
              </span>
            </div>
          )
        }}
      </Match>
      <Match when={props.part.type === "retry" && props.part}>
        {(p) => {
          const r = p() as { attempt: number; error?: { data?: { message?: string } } }
          return (
            <div class="tool">
              <div class="tool-row">
                <span class="mark running" />
                <span class="name">Retry</span>
                <span class="target">
                  {r.error?.data?.message ?? "provider error"} · attempt {r.attempt}
                </span>
              </div>
            </div>
          )
        }}
      </Match>
      <Match when={props.part.type === "compaction"}>
        <div class="compacted-rule">
          <span>Compacted</span>
        </div>
      </Match>
      <Match when={props.part.type === "agent" && props.part}>
        {(p) => <span class="tag">@{(p() as { name: string }).name}</span>}
      </Match>
      <Match when={props.part.type === "subtask" && props.part}>
        {(p) => {
          const t = p() as { agent: string; description: string }
          return (
            <div class="subtask">
              <span class="name">Task</span>
              <span class="t">
                <span class="mono">{t.agent}</span>
                {t.description}
              </span>
            </div>
          )
        }}
      </Match>
    </Switch>
  )
}

function Reasoning(props: { part: Extract<Part, { type: "reasoning" }> }) {
  const [open, setOpen] = createSignal(false)
  const t = () => props.part.time as { start: number; end?: number } | undefined
  return (
    <div class="reasoning" classList={{ open: open() }}>
      <button aria-expanded={open()} onClick={() => setOpen((v) => !v)}>
        <Icon name="chev-right" />
        Reasoning{t()?.end ? ` · ${seconds(t()!.start, t()!.end)}` : "…"}
      </button>
      <Show when={open()}>
        <div class="r-body">{props.part.text}</div>
      </Show>
    </div>
  )
}

/** Tools whose preview is open by default: the result is the point. */
const OPEN_BY_DEFAULT = new Set(["bash", "edit", "write", "apply_patch", "todowrite"])

function ToolView(props: {
  part: ToolPart
  interrupt?: Interrupt
  onOpenSession: (id: string, beside: boolean) => void
}) {
  const st = () => props.part.state as State
  const meta = () => st().metadata ?? {}
  const input = () => st().input ?? {}
  const status = () => st().status
  const [open, setOpen] = createSignal<boolean | null>(null)
  const isOpen = () => open() ?? (OPEN_BY_DEFAULT.has(props.part.tool) || status() === "running")

  const diff = createMemo(() => {
    const d = meta().diff
    return typeof d === "string" && d ? parseUnifiedDiff(d) : null
  })
  const stats = () => {
    const fd = meta().filediff as { additions?: number; deletions?: number } | undefined
    if (fd?.additions != null) return { add: fd.additions, del: fd.deletions ?? 0 }
    const d = diff()
    return d ? diffStats(d) : null
  }
  const output = () => {
    if (props.part.tool === "bash") return (meta().output as string | undefined) ?? st().output ?? ""
    return st().output ?? ""
  }
  const childSession = () =>
    (meta().sessionId as string | undefined) ?? /<task id="([^"]+)"/.exec(st().output ?? "")?.[1] ?? undefined

  const kind = () => {
    if (props.part.tool === "todowrite") return "todos"
    if (props.part.tool === "task") return "task"
    if (props.part.tool === "subagents") return "subagents"
    if (props.part.tool === "workflow") return "workflow"
    if (props.part.tool === "question") return "question"
    if (diff()) return "diff"
    if (props.part.tool === "bash") return "output"
    return "plain"
  }
  const hasBody = () =>
    status() === "error" ||
    kind() === "diff" ||
    (kind() === "output" && !!output()) ||
    (kind() === "plain" && !!st().output && props.part.tool !== "read")

  return (
    <Switch>
      <Match when={kind() === "todos"}>
        <TodoPlate todos={(input().todos as { content: string; status: string }[]) ?? []} />
      </Match>
      <Match when={kind() === "task"}>
        <TaskRow part={props.part} child={childSession()} onOpen={props.onOpenSession} />
      </Match>
      <Match when={kind() === "subagents"}>
        <SubagentsRow part={props.part} child={childSession()} onOpen={props.onOpenSession} />
      </Match>
      <Match when={kind() === "workflow"}>
        <WorkflowRow part={props.part} />
      </Match>
      <Match when={true}>
        <div class="tool" classList={{ open: isOpen() && hasBody() }} data-tool={props.part.id}>
          <Show
            when={hasBody()}
            fallback={
              <div class="tool-row">
                <ToolRowInner part={props.part} stats={stats()} />
              </div>
            }
          >
            <button class="tool-row" aria-expanded={isOpen()} onClick={() => setOpen(!isOpen())}>
              <ToolRowInner part={props.part} stats={stats()} chevron />
            </button>
          </Show>
          <Show when={summary(props.part)}>{(s) => <div class="tool-summary">{s()}</div>}</Show>
          <Show when={isOpen() && hasBody()}>
            <div class="tool-body">
              <Switch>
                <Match when={status() === "error"}>
                  <div class="out fail-out">{toolErrorText(st().error ?? "")}</div>
                </Match>
                <Match when={kind() === "diff"}>
                  <Diff rows={diff()!} />
                </Match>
                <Match when={kind() === "output" || kind() === "plain"}>
                  <OutputBlock text={output()} />
                </Match>
              </Switch>
            </div>
          </Show>
        </div>
      </Match>
    </Switch>
  )
}

function ToolRowInner(props: { part: ToolPart; stats: { add: number; del: number } | null; chevron?: boolean }) {
  const st = () => props.part.state as State
  return (
    <>
      <span class={`mark ${st().status}`} />
      <span class="name">{props.part.tool}</span>
      <span class="target">{toolTarget(props.part) || st().title || ""}</span>
      <span class="meta">
        <Show when={props.stats}>
          {(s) => (
            <>
              <span class="add">+{s().add}</span> <span class="del">−{s().del}</span>
              {st().time?.end ? " · " : ""}
            </>
          )}
        </Show>
        {st().status === "running"
          ? "running"
          : st().status === "pending"
            ? "pending"
            : seconds(st().time?.start, st().time?.end)}
      </span>
      <Show when={props.chevron}>
        <Icon name="chev-right" class="chev" />
      </Show>
    </>
  )
}

/** One-line result under the row, where the full output is not worth opening. */
function summary(part: ToolPart): string {
  const st = part.state as State
  if (st.status !== "completed") return ""
  const out = st.output ?? ""
  switch (part.tool) {
    case "read": {
      const lines = out.split("\n").filter((l) => /^\s*\d+[│|:]/.test(l)).length
      return lines ? `${lines} lines` : ""
    }
    case "grep": {
      const m = /Found (\d+) match/i.exec(out)
      return m ? `${m[1]} matches` : ""
    }
    case "glob": {
      const n = out.split("\n").filter(Boolean).length
      return n ? `${n} files` : ""
    }
    case "bash": {
      const exit = (st.metadata ?? {}).exit
      return typeof exit === "number" && exit !== 0 ? `exit ${exit}` : ""
    }
  }
  return ""
}

function OutputBlock(props: { text: string }) {
  const t = createMemo(() => tail(props.text))
  return (
    <div class="out">
      <Show when={t().hidden}>
        <span class="dim">
          … {t().hidden} earlier lines{"\n"}
        </span>
      </Show>
      {t().text}
    </div>
  )
}

export function TodoPlate(props: { todos: { content: string; status: string }[] }) {
  const done = () => props.todos.filter((t) => t.status === "completed").length
  return (
    <div class="todo-plate">
      <header>
        Todos
        <span class="n">
          {done()}/{props.todos.filter((t) => t.status !== "cancelled").length}
        </span>
      </header>
      <ol>
        <For each={props.todos}>
          {(t, i) => (
            <li class={t.status}>
              <span class="no">{String(i() + 1).padStart(2, "0")}</span>
              <span class="box" />
              <span>{t.content}</span>
            </li>
          )}
        </For>
      </ol>
    </div>
  )
}

const STATE_WORDS: Record<string, string> = { error: "failed", not_started: "not started" }

/** The `subagents` tool: what was asked and where the subagent stands. The output itself is for the model. */
function SubagentsRow(props: { part: ToolPart; child?: string; onOpen: (id: string, beside: boolean) => void }) {
  const st = () => props.part.state as State
  const action = () => String(st().input?.action ?? "")
  const subject = () => shortTitle(st().title ?? String(st().input?.task_id ?? ""))
  const word = () => {
    const value = String(st().metadata?.state ?? "")
    return STATE_WORDS[value] ?? value
  }
  const text = () => {
    if (st().status === "error") return failureLine(st().error ?? "")
    if (action() === "list") return st().metadata?.count === undefined ? "" : `${st().metadata!.count} subagents`
    if (action() === "wait" && st().status !== "completed") return `waiting for ${subject()}`
    return subject()
  }
  return (
    <div class="tool" data-tool={props.part.id}>
      <div class="tool-row">
        <span class={`mark ${st().status}`} />
        <span class="name">{action() === "list" ? "subagents" : `subagent ${action()}`}</span>
        <span class="target">{text()}</span>
        <span class="meta" classList={{ fail: word() === "failed" }}>
          {word()}
        </span>
        <Show when={props.child}>
          {(id) => (
            <button class="link-btn" onClick={() => props.onOpen(id(), true)}>
              Open
            </button>
          )}
        </Show>
      </div>
    </div>
  )
}

/** How often a row in the transcript re-reads a run that is still going. */
const WORKFLOW_ROW_POLL_MS = 2500

/** The `workflow` tool: which run and how it stands, live. The output itself is for the model. */
function WorkflowRow(props: { part: ToolPart }) {
  const st = () => props.part.state as State
  const action = () => String(st().input?.action ?? "")
  const name = () => (st().title ?? "").replace(/^Workflow: /, "") || String(st().input?.name ?? "inline")
  const runID = () => st().metadata?.runID as string | undefined
  const directory = () => state.sessions[(props.part as { sessionID?: string }).sessionID ?? ""]?.directory
  const [run, setRun] = createSignal<WorkflowRun | null>(null)
  const settled = () => {
    const current = run()?.state
    return current === "completed" || current === "failed" || current === "cancelled"
  }
  createEffect(() => {
    const id = runID()
    const dir = directory()
    if (!id || !dir) return
    const read = () => {
      if (settled()) return
      api
        .workflowRun(dir, id)
        .then((next) => setRun(next))
        .catch(() => {
          // A transient failure keeps the last answer.
        })
    }
    read()
    const timer = setInterval(read, WORKFLOW_ROW_POLL_MS)
    onCleanup(() => clearInterval(timer))
  })
  const word = () => {
    if (st().status === "error") return "failed"
    const current = run()?.state ?? String(st().metadata?.state ?? "")
    return current === "cancelled" ? "stopped" : current
  }
  const progress = () => {
    const agents = run()?.agents
    if (!agents?.length) return ""
    const done = agents.filter((a) => a.state === "completed" || a.state === "failed" || a.state === "cancelled").length
    return `${done}/${agents.length} agents · `
  }
  return (
    <div class="tool" data-tool={props.part.id}>
      <div class="tool-row">
        <span class={`mark ${st().status}`} />
        <span class="name">workflow</span>
        <span class="target">{action() === "list" ? "saved workflows" : name()}</span>
        <span class="meta" classList={{ fail: word() === "failed" }}>
          {progress()}
          {word()}
        </span>
        <Show when={action() !== "list"}>
          <a class="link-btn" href="#/workflows">
            Progress
          </a>
        </Show>
      </div>
      <Show when={st().status === "error"}>
        <div class="tool-summary">{failureLine(st().error ?? "")}</div>
      </Show>
    </div>
  )
}

function TaskRow(props: { part: ToolPart; child?: string; onOpen: (id: string, beside: boolean) => void }) {
  const st = () => props.part.state as State
  const input = () => st().input ?? {}
  const child = () => (props.child ? state.sessions[props.child] : undefined)
  const lamp = () => (props.child ? fleetState(state, props.child) : "idle")
  const failed = () => st().status === "error"
  const recovered = () => /<summary>Recovered from errors/.test(st().output ?? "")
  const result = () => {
    if (st().status === "completed") {
      // Output is `<task id=… state=…><task_result>…</task_result></task>`.
      const out = st().output ?? ""
      const inner = /<task_result>([\s\S]*?)<\/task_result>/.exec(out)?.[1] ?? out.replace(/<\/?task[^>]*>/g, "")
      return (
        inner
          .trim()
          .split("\n")
          .find((l) => l.trim()) ?? ""
      )
    }
    if (failed()) return failureLine(st().error ?? "")
    if (!props.child) return ""
    // Between automatic resumes the child sits on its error, the task call still running.
    if (lamp() === "fault") return `Stopped on an error, resuming: ${failureLine(lastError(state, props.child))}`
    const now = activity(state, props.child)
    return [now.kind, now.text].join(" ").trim()
  }
  return (
    <div class="subtask" classList={{ failed: failed() }}>
      <span class="name">Task</span>
      <span class="t">
        <span class="mono">{String(input().subagent_type ?? "")}</span>
        {String(input().description ?? "")}
      </span>
      <Show when={props.child}>
        {(id) => (
          <button class="link-btn" onClick={() => props.onOpen(id(), true)}>
            <span class={`lamp ${lamp()}`} />
            Open
          </button>
        )}
      </Show>
      <span class="res">
        {result() || (child() ? child()!.title : st().status === "running" ? "running…" : "")}
        <Show when={recovered()}>
          <span class="recovered"> · recovered from errors</span>
        </Show>
      </span>
    </div>
  )
}

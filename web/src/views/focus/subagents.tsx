// What the transcript says about subagents: the live list above the composer,
// and the system's notes, which the model reads as text but a person should
// see as what they are.
import { createMemo, For, Show } from "solid-js"
import type { Session } from "../../api/types"
import { fleetState, activity, lastError, type FleetState } from "../../fleet/derive"
import { state } from "../../store/store"
import { Icon } from "../../ui/icons"
import { firstLine, shortTitle, type Note } from "./subagent-notes"

export function NoteView(props: { note: Note; onOpen: (id: string, beside: boolean) => void }) {
  return (
    <Show
      when={props.note.kind === "update" && (props.note as Extract<Note, { kind: "update" }>)}
      fallback={
        <div class="sys-note resume">
          <Icon name="undo" />
          <span>Resumed automatically after an error: {(props.note as Extract<Note, { kind: "resume" }>).error}</span>
        </div>
      }
    >
      {(update) => (
        <div class="sys-note" classList={{ fault: update().state === "error" }}>
          <span class={`lamp ${update().state === "error" ? "fault" : "idle"}`} />
          <span>{update().summary}</span>
          <button class="link-btn" onClick={() => props.onOpen(update().session, true)}>
            Open
          </button>
        </div>
      )}
    </Show>
  )
}

const ACTIVE: FleetState[] = ["working", "retry", "waiting"]

type Row = { session: Session; lamp: FleetState; detail: string }

/** A session's subagents still at work, plus the ones this turn lost. */
function rows(parent: string): Row[] {
  const turns = state.messages[parent] ?? []
  let started = 0
  for (let i = turns.length - 1; i >= 0; i--) {
    const message = state.message[turns[i]!]
    if (message?.role === "user") {
      started = message.time.created
      break
    }
  }
  const parentBusy = ACTIVE.includes(fleetState(state, parent))
  // A failure with its task call still running is on its way to being resumed.
  const awaited = new Set<string>()
  for (const id of turns) {
    for (const pid of state.parts[id] ?? []) {
      const part = state.part[pid] as
        { type?: string; tool?: string; state?: { status?: string; metadata?: { sessionId?: unknown } } } | undefined
      const session = part?.state?.metadata?.sessionId
      if (
        part?.type === "tool" &&
        part.tool === "task" &&
        part.state?.status === "running" &&
        typeof session === "string"
      )
        awaited.add(session)
    }
  }
  return (
    Object.values(state.sessions)
      // A workflow's agents are shown with their workflow.
      .filter(
        (session) =>
          session.parentID === parent && !(session.metadata as { workflowAgent?: unknown } | undefined)?.workflowAgent,
      )
      .map((session) => {
        const lamp = fleetState(state, session.id)
        const now = activity(state, session.id)
        const failure = firstLine(lastError(state, session.id))
        const detail =
          lamp === "fault"
            ? awaited.has(session.id)
              ? `Resuming after an error: ${failure}`
              : failure
            : [now.kind, now.text].join(" ").trim()
        return { session, lamp, detail }
      })
      .filter(
        (row) =>
          ACTIVE.includes(row.lamp) || (row.lamp === "fault" && parentBusy && row.session.time.updated >= started),
      )
      .sort((a, b) => Number(b.lamp !== "fault") - Number(a.lamp !== "fault"))
  )
}

/** The list above the composer: who is working on what, and the todo in progress. */
export function WorkStatus(props: { sessionID: string; onOpen: (id: string, beside: boolean) => void }) {
  const subagents = createMemo(() => rows(props.sessionID))
  const todos = () => state.todos[props.sessionID] ?? []
  const open = () => todos().filter((todo) => todo.status !== "completed" && todo.status !== "cancelled")
  const current = () => open().find((todo) => todo.status === "in_progress") ?? open()[0]
  const done = () => todos().filter((todo) => todo.status === "completed").length
  return (
    <Show when={subagents().length || current()}>
      <div class="work-status">
        <For each={subagents()}>
          {(row) => (
            <div class="work-row">
              <span class={`lamp ${row.lamp}`} />
              <span class="what">{shortTitle(row.session.title)}</span>
              <span class="detail" classList={{ fault: row.lamp === "fault" }}>
                {row.detail}
              </span>
              <button class="link-btn" onClick={() => props.onOpen(row.session.id, true)}>
                Open
              </button>
            </div>
          )}
        </For>
        <Show when={current()}>
          {(todo) => (
            <div class="work-row todo">
              <span class="n">
                {done()}/{todos().filter((item) => item.status !== "cancelled").length}
              </span>
              <span class="what">{todo().content}</span>
              <Show when={open().length > 1}>
                <span class="detail">+{open().length - 1} more</span>
              </Show>
            </div>
          )}
        </Show>
      </div>
    </Show>
  )
}

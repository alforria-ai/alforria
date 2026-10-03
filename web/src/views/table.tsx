// The session table: one live row per session, grouped by project, with
// subagents nested under their parent.
import { For, Show } from "solid-js"
import type { Session } from "../api/types"
import {
  activity,
  contextUse,
  displayTitle,
  filteredGroups,
  fleetState,
  projectName,
  sessionCost,
  todoProgress,
} from "../fleet/fleet"
import type { FleetState } from "../fleet/derive"
import { state } from "../store/store"
import { age, money, now, tildify } from "../ui/format"
import { Icon } from "../ui/icons"
import { server } from "./server"

const STATE_WORD: Record<FleetState, string> = {
  waiting: "Waiting",
  working: "Working",
  retry: "Retry",
  fault: "Fault",
  idle: "Idle",
}

export function StateCell(props: { state: FleetState }) {
  return (
    <span class={`state ${props.state}`}>
      <span class={`lamp ${props.state}`} />
      <span>{STATE_WORD[props.state]}</span>
    </span>
  )
}

/** The label for the "now" cell: a tool name or a verb for the state. */
export function nowLabel(st: FleetState, kind: string) {
  if (st === "waiting") return kind === "question" ? "Asks" : "Approve"
  if (st === "idle") return "Done"
  if (st === "fault") return "Fault"
  if (st === "retry") return "Retry"
  if (kind === "writing") return "Writing"
  if (kind === "thinking") return "Thinking"
  return kind
}

function NowCell(props: { id: string }) {
  const st = () => fleetState(state, props.id)
  const a = () => activity(state, props.id)
  return (
    <div class={`now ${st()} ${a().prose ? "prose" : ""}`}>
      <span class="k">{nowLabel(st(), a().kind)}</span>
      <span class="x" title={a().text}>
        {a().text}
      </span>
    </div>
  )
}

function TodosCell(props: { id: string }) {
  const p = () => todoProgress(state, props.id)
  return (
    <Show
      when={p()}
      fallback={
        <span class="todos">
          <span class="n">—</span>
        </span>
      }
    >
      {(p) => (
        <span class="todos" title={`${p()[0]} of ${p()[1]} todos done`}>
          <Show when={p()[1] <= 10}>
            <span class="cells">
              <For each={Array.from({ length: p()[1] }, (_, i) => i)}>
                {(i) => <i class={i < p()[0] ? "done" : i === p()[0] ? "cur" : ""} />}
              </For>
            </span>
          </Show>
          <span class="n">
            {p()[0]}/{p()[1]}
          </span>
        </span>
      )}
    </Show>
  )
}

export function CtxCell(props: { id: string }) {
  const c = () => contextUse(state, props.id)
  const pct = () => {
    const f = c()?.fraction
    return f == null ? null : Math.round(f * 100)
  }
  return (
    <Show
      when={pct() != null}
      fallback={
        <span class="ctx">
          <span class="n">—</span>
        </span>
      }
    >
      <span class="ctx" title={`Context ${pct()}% used`}>
        <span class={`meter ${pct()! >= 80 ? "high" : ""}`}>
          <i style={{ width: `${Math.min(100, pct()!)}%` }} />
        </span>
        <span class={`n ${pct()! >= 80 ? "high" : ""}`}>{pct()}%</span>
      </span>
    </Show>
  )
}

function SessionRow(props: {
  session: Session
  depth: number
  cursor: boolean
  onOpen: (id: string, beside: boolean) => void
}) {
  const id = () => props.session.id
  const st = () => fleetState(state, id())
  return (
    <tr
      class="s-row"
      classList={{ sub: props.depth > 0, "is-waiting": st() === "waiting", "is-cursor": props.cursor }}
      data-sid={id()}
      tabindex="-1"
      onMouseDown={(e) => e.shiftKey && e.preventDefault()}
      onClick={(e) => props.onOpen(id(), e.shiftKey)}
    >
      <td class="c-state">
        <StateCell state={st()} />
      </td>
      <td class="c-session" title={displayTitle(props.session)}>
        <div class="title-cell">
          <span class="t">{displayTitle(props.session)}</span>
          <Show when={props.session.agent && props.session.agent !== "build"}>
            <span class="tag">{props.session.agent}</span>
          </Show>
        </div>
      </td>
      <td class="c-now">
        <NowCell id={id()} />
      </td>
      <td class="c-todos">
        <TodosCell id={id()} />
      </td>
      <td class="c-ctx">
        <CtxCell id={id()} />
      </td>
      <td class="c-cost r">
        <span class="cost">{money(sessionCost(state, id()))}</span>
      </td>
      <td class="c-age r">
        <span class="age-v">{age(now() - props.session.time.updated)}</span>
      </td>
    </tr>
  )
}

export function SessionTable(props: {
  cursor: string | null
  onOpen: (id: string, beside: boolean) => void
  onNewSession: (projectID: string) => void
}) {
  return (
    <table class="reg" aria-label="Sessions by project">
      <colgroup>
        <col class="c-state" />
        <col class="c-session" />
        <col class="c-now" />
        <col class="c-todos" />
        <col class="c-ctx" />
        <col class="c-cost" />
        <col class="c-age" />
      </colgroup>
      <thead>
        <tr>
          <th>State</th>
          <th>Session</th>
          <th>Now</th>
          <th>Todos</th>
          <th class="h-ctx">Context</th>
          <th class="r">Cost</th>
          <th class="r">Age</th>
        </tr>
      </thead>
      <tbody>
        <For each={filteredGroups()}>
          {(g) => (
            <>
              <tr class="proj-row">
                <td colspan="7">
                  <div class="proj">
                    <h3>{projectName(g.project)}</h3>
                    <span class="path" title={g.project.worktree}>
                      <bdi>{tildify(g.project.worktree, server.home)}</bdi>
                    </span>
                    <Show when={state.branches[g.project.id]}>
                      {(b) => (
                        <span class="branch">
                          <Icon name="fork" />
                          {b()}
                        </span>
                      )}
                    </Show>
                    <span class="tally">
                      {g.rows.length} sessions
                      <Show when={g.rows.filter((r) => fleetState(state, r.session.id) === "waiting").length}>
                        {(w) => (
                          <>
                            {" "}
                            · <b>{w()} waiting</b>
                          </>
                        )}
                      </Show>
                    </span>
                    <button
                      class="add"
                      aria-label={`New session in ${projectName(g.project)}`}
                      onClick={() => props.onNewSession(g.project.id)}
                    >
                      <Icon name="plus" />
                      <span>New session</span>
                    </button>
                  </div>
                </td>
              </tr>
              <For each={g.rows}>
                {(r) => (
                  <SessionRow
                    session={r.session}
                    depth={r.depth}
                    cursor={props.cursor === r.session.id}
                    onOpen={props.onOpen}
                  />
                )}
              </For>
            </>
          )}
        </For>
      </tbody>
    </table>
  )
}

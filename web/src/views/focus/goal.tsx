// The session's goal (`/goal`), above the composer with the rest of its work
// status: what it is, where it stands, and the same controls the slash
// command offers.
import { createSignal, For, Show } from "solid-js"
import { api } from "../../api/client"
import { state } from "../../store/store"
import { toast } from "../toast"
import { goalActions, goalState } from "./goal-notes"

export function GoalRow(props: { sessionID: string }) {
  const [open, setOpen] = createSignal(false)
  // The command answers when the turn it starts ends (resume): don't wait on
  // it; `goal.changed` moves the row.
  const run = (action: string) =>
    void api
      .command(props.sessionID, { command: "goal", arguments: action })
      .catch((err) => toast(`Could not ${action} the goal: ${err instanceof Error ? err.message : err}`, "error"))
  return (
    <Show when={state.goals[props.sessionID]}>
      {(goal) => (
        <>
          <div class="work-row goal">
            <span class={`lamp ${goalState(goal()).lamp}`} />
            <span class={`code ${goal().status}`}>{goalState(goal()).word}</span>
            <Show when={goal().status === "active"}>
              <span class="n" title={`${goal().rounds} of at most ${goal().maxRounds} continuations`}>
                round {goal().rounds}
              </span>
            </Show>
            <button
              class="what"
              aria-expanded={open()}
              title={open() ? "Show less" : goal().objective}
              onClick={() => setOpen(!open())}
            >
              {goal().objective}
            </button>
            <span class="detail" classList={{ fault: goal().status === "paused" }} title={goal().reason}>
              {goal().status === "active" ? "" : goal().reason}
            </span>
            <For each={goalActions(goal())}>
              {(action) => (
                <button class="link-btn" onClick={() => run(action)}>
                  {action}
                </button>
              )}
            </For>
          </div>
          <Show when={open()}>
            <div class="goal-full">
              <p>{goal().objective}</p>
              <Show when={goal().reason}>
                <p class="why">{goal().reason}</p>
              </Show>
            </div>
          </Show>
        </>
      )}
    </Show>
  )
}

/** A quiet word on a session's row: it is working toward a goal, or its goal stopped. */
export function GoalTag(props: { id: string }) {
  const goal = () => state.goals[props.id]
  return (
    <Show when={goal() && goal()!.status !== "complete"}>
      <span class="tag" title={`Goal: ${goal()!.objective}${goal()!.reason ? `\n${goal()!.reason}` : ""}`}>
        {goal()!.status === "paused" ? "goal paused" : "goal"}
      </span>
    </Show>
  )
}

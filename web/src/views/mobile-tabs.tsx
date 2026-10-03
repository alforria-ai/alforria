// Phone navigation: Queue / Sessions / Focus. The current tab is derived from
// the same nav state as the visible pane, so they can never disagree.
import { Show } from "solid-js"
import { Icon } from "../ui/icons"

export function MobileTabs(props: {
  pane: "queue" | "sessions" | "focus" | "settings"
  waiting: number
  canFocus: boolean
  onPane: (p: "queue" | "sessions" | "focus") => void
}) {
  return (
    <nav class="mobile-tabs" aria-label="Views">
      <button aria-current={props.pane === "queue"} onClick={() => props.onPane("queue")}>
        <Icon name="queue" />
        <span>
          Queue
          <Show when={props.waiting}>
            <span class="badge">{props.waiting}</span>
          </Show>
        </span>
      </button>
      <button aria-current={props.pane === "sessions"} onClick={() => props.onPane("sessions")}>
        <Icon name="rows" />
        <span>Sessions</span>
      </button>
      <button aria-current={props.pane === "focus"} disabled={!props.canFocus} onClick={() => props.onPane("focus")}>
        <Icon name="columns" />
        <span>Focus</span>
      </button>
    </nav>
  )
}

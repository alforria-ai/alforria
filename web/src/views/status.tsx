// The status line: link state, fleet counts, today's totals. Mono data only.
import { Match, Switch } from "solid-js"
import { counts, totals } from "../fleet/fleet"
import { state } from "../store/store"
import { ktok, money, now } from "../ui/format"

export function StatusLine(props: { onKeys: () => void }) {
  const since = () => ((now() - state.conn.lastFrame) / 1000).toFixed(0)
  return (
    <footer class="status" id="status">
      <span title="Event stream">
        <i class="link-dot" classList={{ off: state.conn.state !== "live" }} />
        <Switch>
          <Match when={state.conn.state === "live"}>LINKED /global/event · last frame {since()}s</Match>
          <Match when={state.conn.state === "connecting"}>CONNECTING…</Match>
          <Match when={state.conn.state === "retrying"}>
            RECONNECTING · attempt {state.conn.attempt}
            {state.conn.retryAt > now() ? ` in ${Math.ceil((state.conn.retryAt - now()) / 1000)}s` : ""}
          </Match>
        </Switch>
      </span>
      <span>
        {counts().all} sessions · {counts().working} working · {counts().waiting} waiting
      </span>
      <span>{ktok(totals().tokens)} tokens</span>
      <span>{money(totals().cost)}</span>
      <span class="spacer" />
      <button onClick={props.onKeys} aria-label="Keyboard shortcuts">
        ? Keys
      </button>
    </footer>
  )
}

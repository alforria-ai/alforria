// The session list shown beside focus columns: every session, live, grouped by
// project. Click swaps the active column; shift-click or "open beside" adds a
// column. Collapsed, it becomes a strip of status lamps.
import { For, Show } from "solid-js"
import { activity, counts, fleetState, groups, interrupts, projectName } from "../fleet/fleet"
import { nav } from "../nav/route"
import { state } from "../store/store"
import { age, now } from "../ui/format"
import { Icon } from "../ui/icons"
import { nowLabel } from "./table"

export function Rail(props: {
  onOpen: (id: string, beside: boolean) => void
  onNextWaiting: () => void
  onCollapse: () => void
}) {
  const oldest = () => interrupts()[0]
  const colsOf = (id: string) =>
    nav.columns.map((c, i) => (c.kind === "session" && c.session === id ? i : -1)).filter((i) => i >= 0)
  return (
    <aside class="queue rail" id="queue" aria-label="Sessions">
      <div class="pane-head">
        <h2 id="railTitle">Sessions</h2>
        <span class="count">
          {counts().working} working · {counts().all} total
        </span>
        <span class="spacer" />
        <button class="icon-btn collapse" aria-label="Collapse session list" onClick={props.onCollapse}>
          <Icon name="panel" />
        </button>
      </div>
      <Show when={oldest()}>
        {(o) => (
          <button class="rail-next" onClick={props.onNextWaiting}>
            <span class="lamp waiting" />
            <span>
              <b>{interrupts().length} waiting</b> · open {state.sessions[o().sessionID]?.title || "session"}
            </span>
            <kbd>W</kbd>
          </button>
        )}
      </Show>
      <div class="rail-list" role="listbox" aria-labelledby="railTitle">
        <For each={groups()}>
          {(g) => (
            <>
              <div class="rail-proj">
                <h3>{projectName(g.project)}</h3>
                <span class="tally">
                  {g.rows.length}
                  <Show when={g.rows.filter((r) => fleetState(state, r.session.id) === "waiting").length}>
                    {(w) => (
                      <>
                        {" "}
                        · <b>{w()} waiting</b>
                      </>
                    )}
                  </Show>
                </span>
              </div>
              <For each={g.rows}>
                {(r) => {
                  const id = r.session.id
                  const st = () => fleetState(state, id)
                  const a = () => activity(state, id)
                  const cols = () => colsOf(id)
                  return (
                    <div
                      class={`rail-row ${st()}`}
                      classList={{
                        sub: r.depth > 0,
                        "is-active": cols().includes(nav.active),
                        "is-open": cols().length > 0,
                      }}
                      role="option"
                      aria-selected={cols().includes(nav.active)}
                      tabindex="-1"
                      title={r.session.title}
                      data-rail={id}
                      onClick={(e) => props.onOpen(id, e.shiftKey)}
                    >
                      <span class={`lamp ${st()}`} />
                      <span class="t">{r.session.title || "Untitled session"}</span>
                      <span class="badges">
                        <For each={cols()}>
                          {(i) => (
                            <i classList={{ on: i === nav.active }} title={`Column ${i + 1}`}>
                              {i + 1}
                            </i>
                          )}
                        </For>
                      </span>
                      <span class="age-v">{age(now() - r.session.time.updated)}</span>
                      <span class="now" classList={{ prose: a().prose }}>
                        <b>{nowLabel(st(), a().kind)}</b> {a().text}
                      </span>
                      <button
                        class="beside"
                        aria-label={`Open ${r.session.title} beside`}
                        title="Open beside (⇧ click)"
                        onClick={(e) => {
                          e.stopPropagation()
                          props.onOpen(id, true)
                        }}
                      >
                        <Icon name="columns" />
                      </button>
                    </div>
                  )
                }}
              </For>
            </>
          )}
        </For>
      </div>
      <div class="rail-hint">
        Click to switch the active column · ⇧ click to open beside · <kbd>Alt</kbd>
        <kbd>↑</kbd>
        <kbd>↓</kbd>
      </div>
      <div class="q-rail">
        <button class="icon-btn" aria-label="Expand session list" onClick={props.onCollapse}>
          <Icon name="panel" />
        </button>
        <For each={groups().flatMap((g) => g.rows)}>
          {(r) => (
            <button
              class="mini"
              classList={{
                on:
                  nav.columns[nav.active]?.kind === "session" &&
                  (nav.columns[nav.active] as { session: string }).session === r.session.id,
              }}
              title={`${r.session.title} · ${fleetState(state, r.session.id)}`}
              onClick={(e) => props.onOpen(r.session.id, e.shiftKey)}
            >
              <span class={`lamp ${fleetState(state, r.session.id)}`} />
            </button>
          )}
        </For>
      </div>
    </aside>
  )
}

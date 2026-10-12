// The session list shown beside focus columns: every session, live, grouped by
// project. Click swaps the active column; shift-click or "open beside" adds a
// column. Collapsed, it becomes a strip of status lamps. The list is one tab
// stop (roving tabindex): ↑/↓, Home and End move between rows, Enter opens,
// Shift+Enter opens beside.
import { createSignal, For, Show } from "solid-js"
import { activity, counts, displayTitle, fleetState, groups, interrupts } from "../fleet/fleet"
import { nav } from "../nav/route"
import { state } from "../store/store"
import { age, now } from "../ui/format"
import { Icon } from "../ui/icons"
import { nowLabel } from "./table"

export function Rail(props: {
  onOpen: (id: string, beside: boolean) => void
  onNextWaiting: () => void
  onCollapse: () => void
  /** Rename, archive or delete (a row's context menu, or F2 on it). */
  onManage: (id: string) => void
}) {
  const oldest = () => interrupts()[0]
  const colsOf = (id: string) =>
    nav.columns.map((c, i) => (c.kind === "session" && c.session === id ? i : -1)).filter((i) => i >= 0)
  const ids = () => groups().flatMap((g) => g.rows.map((r) => r.session.id))
  // The row that takes the list's one tab stop: the last one moved to, else
  // the active column's session, else the first.
  const [roving, setRoving] = createSignal<string | null>(null)
  const tabStop = () => {
    const list = ids()
    const r = roving()
    if (r && list.includes(r)) return r
    const col = nav.columns[nav.active]
    if (col?.kind === "session" && list.includes(col.session)) return col.session
    return list[0]
  }
  let listEl!: HTMLDivElement
  const onListKey = (e: KeyboardEvent) => {
    if (e.metaKey || e.ctrlKey || e.altKey) return
    const id = (e.target as HTMLElement).closest<HTMLElement>("[data-rail]")?.dataset.rail
    if (!id) return
    if (e.key === "Enter" || e.key === " ") {
      e.preventDefault()
      return props.onOpen(id, e.shiftKey)
    }
    const list = ids()
    const at = list.indexOf(id)
    const to =
      e.key === "ArrowDown"
        ? list[Math.min(list.length - 1, at + 1)]
        : e.key === "ArrowUp"
          ? list[Math.max(0, at - 1)]
          : e.key === "Home"
            ? list[0]
            : e.key === "End"
              ? list[list.length - 1]
              : undefined
    if (!to) return
    e.preventDefault()
    setRoving(to)
    listEl.querySelector<HTMLElement>(`[data-rail="${to}"]`)?.focus()
  }
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
              <b>{interrupts().length} waiting</b> · open {displayTitle(state.sessions[o().sessionID])}
            </span>
            <kbd>W</kbd>
          </button>
        )}
      </Show>
      <div class="rail-list" role="listbox" aria-labelledby="railTitle" ref={listEl} onKeyDown={onListKey}>
        <For each={groups()}>
          {(g) => (
            // A listbox may only hold options and groups: each project is a
            // labelled group, its header presentational.
            <div role="group" aria-label={g.name}>
              <div class="rail-proj" role="presentation">
                <h3 role="presentation">{g.name}</h3>
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
                      tabindex={tabStop() === id ? 0 : -1}
                      title={displayTitle(r.session)}
                      data-rail={id}
                      onFocus={() => setRoving(id)}
                      onContextMenu={(e) => {
                        e.preventDefault()
                        props.onManage(id)
                      }}
                      onMouseDown={(e) => e.shiftKey && e.preventDefault()}
                      onClick={(e) => props.onOpen(id, e.shiftKey)}
                    >
                      <span class={`lamp ${st()}`} />
                      <span class="t">{displayTitle(r.session)}</span>
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
                      {/* An option holds no controls: this is the pointer's
                          shortcut for Shift+Enter / ⇧ click. */}
                      <span
                        class="beside"
                        aria-hidden="true"
                        title="Open beside (⇧ click)"
                        onClick={(e) => {
                          e.stopPropagation()
                          props.onOpen(id, true)
                        }}
                      >
                        <Icon name="columns" />
                      </span>
                    </div>
                  )
                }}
              </For>
            </div>
          )}
        </For>
      </div>
      <div class="rail-hint">
        Click to switch the active column · ⇧ click or <kbd>⇧</kbd>
        <kbd>↵</kbd> to open beside · <kbd>Alt</kbd>
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
              title={`${displayTitle(r.session)} · ${fleetState(state, r.session.id)}`}
              onMouseDown={(e) => e.shiftKey && e.preventDefault()}
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

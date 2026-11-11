// The session list shown beside focus columns: every session, live, grouped by
// project. Click swaps the active column; shift-click or "open beside" adds a
// column. Collapsed, it becomes a strip of status lamps. The list is one tab
// stop (roving tabindex): ↑/↓, Home and End move between rows, Enter opens,
// Shift+Enter opens beside, ← and → hide and show a parent's subagents.
import { createSignal, For, Show } from "solid-js"
import { activity, counts, displayTitle, fleetState, interrupts, listedGroups, subagentSummary } from "../fleet/fleet"
import { subagentsCollapsed } from "../nav/collapse"
import { nav } from "../nav/route"
import { unseen } from "../nav/unseen"
import { autoAccepting } from "./queue/auto"
import { state } from "../store/store"
import { age, now } from "../ui/format"
import { Icon } from "../ui/icons"
import { GoalTag } from "./focus/goal"
import { nowLabel } from "./table"

/** A session that finished or failed while nobody was looking: an ink block and a word. */
export function UnseenMark(props: { id: string }) {
  return (
    <Show when={unseen[props.id]}>
      {(u) => (
        <span
          class="unseen-mark"
          classList={{ fault: u().outcome === "fault" }}
          title={u().outcome === "fault" ? "Failed while you were away" : "Finished while you were away"}
        >
          <span class="sr-only">{u().outcome === "fault" ? "New failure: " : "New: "}</span>
        </span>
      )}
    </Show>
  )
}

/** Permissions for this session are allowed without asking (per-session auto-accept). */
export function AutoTag(props: { id: string }) {
  return (
    <Show when={autoAccepting(props.id)}>
      <span class="auto-tag" title="Auto-accepting permissions from this page">
        auto
      </span>
    </Show>
  )
}

/**
 * A parent's subagents, shown or hidden. An option holds no controls: this is
 * the pointer's shortcut for → and ←, which the list announces itself.
 */
function RailFold(props: { id: string; subagents: string[]; onFold: (id: string) => void }) {
  const folded = () => subagentsCollapsed(props.id)
  const sum = () => subagentSummary(props.subagents)
  return (
    <span
      class="fold"
      classList={{ folded: folded(), waiting: folded() && sum().waiting > 0 }}
      aria-hidden="true"
      title={`${folded() ? "Show" : "Hide"} subagents (${folded() ? "→" : "←"}): ${sum().text}`}
      onMouseDown={(e) => e.preventDefault()}
      onClick={(e) => {
        e.stopPropagation()
        props.onFold(props.id)
      }}
    >
      <Icon name={folded() ? "chev-right" : "chev-down"} />
      <span>{sum().count}</span>
    </span>
  )
}

export function Rail(props: {
  onOpen: (id: string, beside: boolean) => void
  onNextWaiting: () => void
  onCollapse: () => void
  /** Rename, archive or delete (a row's context menu, or F2 on it). */
  onManage: (id: string) => void
  /** Every session, however old. */
  onHistory: () => void
  /** Show or hide a parent's subagents. */
  onFold: (id: string, collapsed?: boolean) => void
}) {
  const oldest = () => interrupts()[0]
  const colsOf = (id: string) =>
    nav.columns.map((c, i) => (c.kind === "session" && c.session === id ? i : -1)).filter((i) => i >= 0)
  const ids = () => listedGroups().flatMap((g) => g.rows.map((r) => r.session.id))
  const rowOf = (id: string) =>
    listedGroups()
      .flatMap((g) => g.rows)
      .find((r) => r.session.id === id)
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
    // A tree's keys: → shows a parent's subagents, ← hides them, and from a
    // subagent ← steps up to its parent.
    if (e.key === "ArrowRight" || e.key === "ArrowLeft") {
      const show = e.key === "ArrowRight"
      const row = rowOf(id)
      if (row?.subagents.length && subagentsCollapsed(id) === show) {
        e.preventDefault()
        return props.onFold(id, !show)
      }
      const parent = state.sessions[id]?.parentID
      if (show || !parent || !rowOf(parent)) return
      e.preventDefault()
      setRoving(parent)
      return listEl.querySelector<HTMLElement>(`[data-rail="${parent}"]`)?.focus()
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
        <button
          class="icon-btn"
          aria-label="Session history"
          title="Session history: every session, however old (H)"
          onClick={props.onHistory}
        >
          <Icon name="history" />
        </button>
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
        <For each={listedGroups()}>
          {(g) => (
            // A listbox may only hold options and groups: each project is a
            // labelled group, its header presentational.
            <div role="group" aria-label={g.name}>
              <div class="rail-proj" role="presentation">
                <h3 role="presentation">{g.name}</h3>
                <span class="tally">
                  {g.all.length}
                  <Show when={g.all.filter((r) => fleetState(state, r.session.id) === "waiting").length}>
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
                      style={r.depth > 1 ? { "--depth": r.depth } : undefined}
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
                      <span class="t">
                        <UnseenMark id={id} />
                        {displayTitle(r.session)}
                        <Show when={r.subagents.length}>
                          <span class="sr-only">
                            , {subagentSummary(r.subagents).text}, {subagentsCollapsed(id) ? "hidden" : "shown"}
                          </span>
                        </Show>
                      </span>
                      <span class="badges">
                        <Show when={r.subagents.length}>
                          <RailFold id={id} subagents={r.subagents} onFold={props.onFold} />
                        </Show>
                        <GoalTag id={id} />
                        <AutoTag id={id} />
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
        <For each={listedGroups().flatMap((g) => g.rows)}>
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

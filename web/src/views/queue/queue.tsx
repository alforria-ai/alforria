// The interrupt queue: everything waiting on the human, oldest first. The
// oldest item is open; one key clears it and the next opens.
import { createEffect, createMemo, createRoot, createSignal, For, on, onMount, Show } from "solid-js"
import type { Interrupt } from "../../fleet/derive"
import { counts, interrupts } from "../../fleet/fleet"
import { Icon } from "../../ui/icons"
import { act, ledger, selected, setSelected, stamped } from "./act"
import { Slip, slipKeys, slipSummary } from "./slip"
import { state } from "../../store/store"

const memos = createRoot(() => {
  /** Live items plus ones still showing their verdict stamp, in age order. */
  const queueItems = createMemo(() => {
    const live = interrupts()
    const ids = new Set(live.map((i) => i.id))
    const leaving = Object.values(stamped)
      .map((s) => s.interrupt)
      .filter((i) => !ids.has(i.id))
    return [...live, ...leaving].sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0))
  })

  const activeItem = createMemo<Interrupt | undefined>(() => {
    const items = queueItems().filter((i) => !stamped[i.id])
    return items.find((i) => i.id === selected()) ?? items[0]
  })
  return { queueItems, activeItem }
})
export const { queueItems, activeItem } = memos

export function Queue(props: {
  onOpenSession: (id: string) => void
  onCollapse: () => void
  ref?: (el: HTMLElement) => void
}) {
  let listEl!: HTMLDivElement
  // Items present when the queue first renders don't animate in; later arrivals do.
  const seen = new Set<string>()
  const [booted, setBooted] = createSignal(false)
  onMount(() => setTimeout(() => setBooted(true), 1500))
  const arriving = (id: string) => {
    const fresh = booted() && !seen.has(id)
    seen.add(id)
    return fresh
  }

  const move = (d: number) => {
    const items = queueItems().filter((i) => !stamped[i.id])
    const at = items.findIndex((i) => i.id === activeItem()?.id)
    const next = items[Math.max(0, Math.min(items.length - 1, at + d))]
    if (next) {
      setSelected(next.id)
      focusActive()
    }
  }
  const focusActive = () =>
    queueMicrotask(() => listEl?.querySelector<HTMLElement>(".slip.is-active:not(.is-stamped)")?.focus())

  // A verdict key only acts on an item that has been on screen long enough to
  // read: after a verdict the next item slides in under the same finger, and a
  // new arrival can replace the active item. Auto-repeat never acts.
  const READ_DELAY = 500
  let activeSince = Date.now()
  createEffect(
    on(
      () => activeItem()?.id,
      () => (activeSince = Date.now()),
    ),
  )
  const settled = (e: KeyboardEvent) => {
    if (e.repeat || Date.now() - activeSince < READ_DELAY) {
      e.preventDefault()
      return false
    }
    return true
  }

  const onKey = (e: KeyboardEvent) => {
    if (e.metaKey || e.ctrlKey || e.altKey) return
    const t = e.target as HTMLElement
    if (t.tagName === "INPUT" || t.tagName === "TEXTAREA") return
    const item = activeItem()
    const k = e.key.toLowerCase()
    if (k === "j" || e.key === "ArrowDown") return (e.preventDefault(), move(1))
    if (k === "k" || e.key === "ArrowUp") return (e.preventDefault(), move(-1))
    if (!item) return
    if (k === "o") return (e.preventDefault(), props.onOpenSession(item.sessionID))
    const what = `${state.sessions[item.sessionID]?.title ?? ""}: ${slipSummary(item)}`
    if (item.kind === "permission") {
      const v = k === "a" ? "once" : k === "s" ? "always" : k === "d" ? "reject" : null
      if (v && settled(e)) {
        e.preventDefault()
        void act(item, v, { what }).then(focusActive)
      }
      return
    }
    const slip = slipKeys(listEl.querySelector<HTMLElement>(`[data-slip="${item.id}"]`))
    if (/^[1-9]$/.test(e.key)) return (e.preventDefault(), slip?.pick(Number(e.key) - 1))
    if (e.key === "Enter") return settled(e) && (e.preventDefault(), slip?.submit() && focusActive())
    if (k === "x") return settled(e) && (e.preventDefault(), void act(item, "dismiss", { what }).then(focusActive))
  }

  return (
    <aside class="queue" id="queue" aria-label="Waiting on you" ref={props.ref} onKeyDown={onKey}>
      <div class="pane-head">
        <h2 id="queueTitle">Waiting on you</h2>
        <span class="count">{interrupts().length} · oldest first</span>
        <span class="spacer" />
        <button class="icon-btn collapse" aria-label="Collapse queue" onClick={props.onCollapse}>
          <Icon name="panel" />
        </button>
      </div>
      <div class="q-list" role="list" aria-labelledby="queueTitle" ref={listEl}>
        <Show
          when={queueItems().length}
          fallback={
            <div class="q-clear">
              <div class="big">All clear</div>
              <p>Nothing is waiting on you. New approvals and questions land here, oldest first.</p>
              <dl>
                <dt>Working</dt>
                <dd>{counts().working}</dd>
                <dt>Idle</dt>
                <dd>{counts().idle + counts().fault}</dd>
                <dt>Cleared</dt>
                <dd>{ledger.length}</dd>
                <dt>Last</dt>
                <dd>{ledger[0]?.time ?? "—"}</dd>
              </dl>
            </div>
          }
        >
          <For each={queueItems()}>
            {(item) => (
              <Slip
                interrupt={item}
                active={activeItem()?.id === item.id || !!stamped[item.id]}
                arriving={arriving(item.id)}
                onExpand={() => {
                  setSelected(item.id)
                  focusActive()
                }}
                onOpenSession={props.onOpenSession}
              />
            )}
          </For>
        </Show>
      </div>
      <Show when={ledger.length}>
        <details class="q-ledger">
          <summary>
            <Icon name="chev-right" />
            <span class="label">Cleared</span>
            <span class="mono">{ledger.length}</span>
          </summary>
          <ol>
            <For each={ledger.slice(0, 12)}>
              {(l) => (
                <li>
                  <span>{l.time}</span>
                  <b>{l.label}</b>
                  <span>{l.what}</span>
                </li>
              )}
            </For>
          </ol>
        </details>
      </Show>
      <div class="q-rail">
        <button class="icon-btn" aria-label="Expand queue" onClick={props.onCollapse}>
          <Icon name="panel" />
        </button>
        <span class="n" classList={{ lit: interrupts().length > 0 }} title={`${interrupts().length} waiting`}>
          {interrupts().length}
        </span>
      </div>
    </aside>
  )
}

/** Focus the oldest open item (the global Q key). */
export function focusQueue(root: HTMLElement | undefined) {
  setSelected(null)
  queueMicrotask(() => root?.querySelector<HTMLElement>(".slip.is-active:not(.is-stamped)")?.focus())
}

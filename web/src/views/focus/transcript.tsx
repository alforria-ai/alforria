// A session transcript. Messages group into turns (a prompt and the assistant
// steps that answer it); each pending permission or question renders inline
// right after the tool call that raised it. The view sticks to the bottom
// while the reader is there, and offers "Latest" when they have scrolled up.
import { createMemo, createSignal, For, onCleanup, onMount, Show } from "solid-js"
import type { AssistantMessage, UserMessage } from "../../api/types"
import { fleetState, interruptsFor, type Interrupt } from "../../fleet/derive"
import { state } from "../../store/store"
import { hm, ktok, money } from "../../ui/format"
import { Icon } from "../../ui/icons"
import { Slip } from "../queue/slip"
import { PartView } from "./parts"

/** Arrays of ids compare by content so memos only notify on real changes. */
const sameIds = (a: string[], b: string[]) => a.length === b.length && a.every((x, i) => x === b[i])

/**
 * A turn is keyed by its first message id (the prompt, or the first reply when
 * the transcript starts mid-turn). Rendering by these string keys keeps every
 * turn's DOM, highlight state, open tool rows and half-typed answers alive
 * while messages stream and update.
 */
function turnKeys(sessionID: string): string[] {
  const keys: string[] = []
  for (const id of state.messages[sessionID] ?? []) {
    const m = state.message[id]
    if (!m) continue
    if (m.role === "user" || !keys.length) keys.push(id)
  }
  return keys
}

/** The message ids of one turn: its prompt (if any) and the replies after it. */
function turnIds(sessionID: string, key: string): { user?: string; replies: string[] } {
  const ids = state.messages[sessionID] ?? []
  const start = ids.indexOf(key)
  const out: { user?: string; replies: string[] } = { replies: [] }
  for (let i = Math.max(0, start); i < ids.length; i++) {
    const m = state.message[ids[i]!]
    if (!m) continue
    if (m.role === "user") {
      if (i !== start) break
      out.user = m.id
    } else out.replies.push(m.id)
  }
  return out
}

export function Transcript(props: { sessionID: string; onOpenSession: (id: string, beside: boolean) => void }) {
  let scroller!: HTMLDivElement
  let content!: HTMLDivElement
  const [stuck, setStuck] = createSignal(true)
  // Long sessions render their most recent turns; older ones on request.
  const [shown, setShown] = createSignal(40)
  const turns = createMemo(() => turnKeys(props.sessionID), [], { equals: sameIds })
  const pending = createMemo(() => interruptsFor(state, props.sessionID))
  const pendingById = createMemo(() => new Map(pending().map((i) => [i.id, i])))
  const busy = () => fleetState(state, props.sessionID) === "working"

  // Interrupts tied to a tool call render after that call; the rest go last.
  const byCall = createMemo(() => {
    const m = new Map<string, Interrupt>()
    for (const i of pending()) {
      const call = i.request.tool?.callID
      if (call) m.set(call, i)
    }
    return m
  })
  const orphans = createMemo(
    () => {
      const calls = new Set<string>()
      for (const mid of state.messages[props.sessionID] ?? [])
        for (const pid of state.parts[mid] ?? []) {
          const p = state.part[pid]
          if (p?.type === "tool") calls.add(p.callID)
        }
      return pending()
        .filter((i) => !i.request.tool?.callID || !calls.has(i.request.tool.callID))
        .map((i) => i.id)
    },
    [],
    { equals: sameIds },
  )

  const atBottom = () => scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 32
  const toBottom = () => (scroller.scrollTop = scroller.scrollHeight)
  onMount(() => {
    toBottom()
    const ro = new ResizeObserver(() => stuck() && toBottom())
    ro.observe(content)
    onCleanup(() => ro.disconnect())
  })

  return (
    <div class="transcript" ref={scroller} tabindex="0" aria-label="Transcript" onScroll={() => setStuck(atBottom())}>
      <div ref={content}>
        <Show when={state.loaded[props.sessionID]} fallback={<div class="transcript-loading">Loading transcript…</div>}>
          <Show
            when={turns().length}
            fallback={
              <div class="q-empty">
                <h3>New session</h3>
                <p>Write the first prompt below.</p>
              </div>
            }
          >
            <Show when={turns().length > shown()}>
              <button
                class="link-btn earlier"
                onClick={() => {
                  // Keep the reader's place: grow the window, then restore the offset from the bottom.
                  const fromBottom = scroller.scrollHeight - scroller.scrollTop
                  setStuck(false)
                  setShown((w) => w + 40)
                  requestAnimationFrame(() => (scroller.scrollTop = scroller.scrollHeight - fromBottom))
                }}
              >
                Show {Math.min(40, turns().length - shown())} earlier turns
              </button>
            </Show>
            <For each={turns().slice(-shown())}>
              {(key) => (
                <TurnView
                  sessionID={props.sessionID}
                  turnKey={key}
                  last={key === turns()[turns().length - 1]}
                  busy={busy()}
                  byCall={byCall()}
                  onOpenSession={props.onOpenSession}
                />
              )}
            </For>
          </Show>
          <For each={orphans()}>
            {(id) => (
              <Show when={pendingById().get(id)}>
                {(i) => <Slip interrupt={i()} active inline onOpenSession={(sid) => props.onOpenSession(sid, false)} />}
              </Show>
            )}
          </For>
        </Show>
      </div>
      <Show when={!stuck()}>
        <button
          class="latest show"
          onClick={() => {
            setStuck(true)
            toBottom()
          }}
        >
          <Icon name="arrow-down" />
          Latest
        </button>
      </Show>
    </div>
  )
}

function TurnView(props: {
  sessionID: string
  turnKey: string
  last: boolean
  busy: boolean
  byCall: Map<string, Interrupt>
  onOpenSession: (id: string, beside: boolean) => void
}) {
  const ids = createMemo(() => turnIds(props.sessionID, props.turnKey), undefined, {
    equals: (a, b) => !!a && a.user === b.user && sameIds(a.replies, b.replies),
  })
  const user = () => {
    const id = ids().user
    return id ? (state.message[id] as UserMessage | undefined) : undefined
  }
  const replies = () => ids().replies
  const reply = (id: string) => state.message[id] as AssistantMessage | undefined
  const head = () => {
    const first = replies()[0]
    return first ? reply(first) : undefined
  }
  const totals = createMemo(() => {
    let tokens = 0
    let cost = 0
    for (const id of replies()) {
      const r = reply(id)
      if (!r) continue
      tokens += r.tokens.input + r.tokens.output + r.tokens.reasoning + r.tokens.cache.read + r.tokens.cache.write
      cost += r.cost
    }
    const first = head()?.time.created
    const lastId = replies()[replies().length - 1]
    const end = lastId ? reply(lastId)?.time.completed : undefined
    return { tokens, cost, ms: first && end ? end - first : 0 }
  })
  const streaming = () => props.last && props.busy
  return (
    <>
      <Show when={user()}>
        {(u) => (
          <div class="msg msg-user">
            <div class="msg-label">
              You<span class="mono">{hm(u().time.created)}</span>
            </div>
            <div class="body">
              <For each={state.parts[u().id] ?? []}>
                {(pid) => (
                  <Show when={state.part[pid]}>
                    {(p) => (
                      <Show
                        when={p().type === "text"}
                        fallback={<PartView part={p()} streaming={false} onOpenSession={props.onOpenSession} />}
                      >
                        <Show when={!(p() as { synthetic?: boolean }).synthetic}>
                          <div class="user-text">{(p() as { text: string }).text}</div>
                        </Show>
                      </Show>
                    )}
                  </Show>
                )}
              </For>
            </div>
          </div>
        )}
      </Show>
      <Show when={head()}>
        {(h) => (
          <div class="msg msg-asst">
            <div class="msg-label">
              {h().agent}
              <span class="mono">
                {h().modelID} · {hm(h().time.created)}
              </span>
            </div>
            <div class="parts">
              <For each={replies()}>
                {(rid) => (
                  <>
                    <For each={state.parts[rid] ?? []}>
                      {(pid) => (
                        <Show when={state.part[pid]}>
                          {(p) => (
                            <>
                              <PartView part={p()} streaming={streaming()} onOpenSession={props.onOpenSession} />
                              <Show when={p().type === "tool" && props.byCall.get((p() as { callID: string }).callID)}>
                                {(i) => (
                                  <Slip
                                    interrupt={i()}
                                    active
                                    inline
                                    onOpenSession={(id) => props.onOpenSession(id, false)}
                                  />
                                )}
                              </Show>
                            </>
                          )}
                        </Show>
                      )}
                    </For>
                    <Show when={reply(rid)?.error}>
                      {(err) => <ErrorPlate error={err()} sessionID={props.sessionID} />}
                    </Show>
                  </>
                )}
              </For>
              <Show when={streaming()}>
                <div class="prose">
                  <span class="caret" />
                </div>
              </Show>
            </div>
            <Show when={!streaming() && totals().tokens}>
              <div class="step-meta">
                {ktok(totals().tokens)} tokens · {money(totals().cost)}
                {totals().ms ? ` · ${formatMs(totals().ms)}` : ""}
              </div>
            </Show>
          </div>
        )}
      </Show>
    </>
  )
}

function formatMs(ms: number) {
  const s = Math.round(ms / 1000)
  return s < 60 ? `${s}s` : `${Math.floor(s / 60)}m ${String(s % 60).padStart(2, "0")}s`
}

const RECOVERABLE: Record<string, string> = {
  ContextOverflowError:
    "The conversation no longer fits the model's context. Compact it to a summary, or fork and continue.",
  MessageOutputLengthError: "The reply hit the model's output limit. Ask it to continue, or compact first.",
  ProviderAuthError: "The provider rejected the credentials. Check them in Settings → Providers.",
}

function ErrorPlate(props: { error: NonNullable<AssistantMessage["error"]>; sessionID: string }) {
  const message = () => (props.error as { data?: { message?: string } }).data?.message ?? ""
  return (
    <Show
      when={props.error.name !== "MessageAbortedError"}
      fallback={
        <div class="verdict-row">
          <b>Stopped</b>
          <span>the turn was interrupted</span>
        </div>
      }
    >
      <div class="fault-plate">
        <header>
          Fault<span class="mono">{props.error.name}</span>
        </header>
        <p>{RECOVERABLE[props.error.name] ?? message()}</p>
        <Show when={RECOVERABLE[props.error.name] && message()}>
          <p class="fault-detail mono">{message()}</p>
        </Show>
      </div>
    </Show>
  )
}

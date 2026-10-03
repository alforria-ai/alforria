// A session transcript. Messages group into turns (a prompt and the assistant
// steps that answer it); each pending permission or question renders inline
// right after the tool call that raised it. The view sticks to the bottom
// while the reader is there, and offers "Latest" when they have scrolled up.
import { createMemo, createSignal, For, onCleanup, onMount, Show } from "solid-js"
import type { AssistantMessage, Message, UserMessage } from "../../api/types"
import { fleetState, interruptsFor, type Interrupt } from "../../fleet/derive"
import { state } from "../../store/store"
import { hm, ktok, money } from "../../ui/format"
import { Icon } from "../../ui/icons"
import { Slip } from "../queue/slip"
import { PartView } from "./parts"

interface Turn {
  user?: UserMessage
  replies: AssistantMessage[]
}

function turnsOf(ids: string[]): Turn[] {
  const turns: Turn[] = []
  for (const id of ids) {
    const m: Message | undefined = state.message[id]
    if (!m) continue
    if (m.role === "user") turns.push({ user: m, replies: [] })
    else {
      const last = turns[turns.length - 1]
      if (last) last.replies.push(m)
      else turns.push({ replies: [m] })
    }
  }
  return turns
}

export function Transcript(props: { sessionID: string; onOpenSession: (id: string, beside: boolean) => void }) {
  let scroller!: HTMLDivElement
  let content!: HTMLDivElement
  const [stuck, setStuck] = createSignal(true)
  const turns = createMemo(() => turnsOf(state.messages[props.sessionID] ?? []))
  const pending = createMemo(() => interruptsFor(state, props.sessionID))
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
  const orphans = createMemo(() => {
    const calls = new Set<string>()
    for (const mid of state.messages[props.sessionID] ?? [])
      for (const pid of state.parts[mid] ?? []) {
        const p = state.part[pid]
        if (p?.type === "tool") calls.add(p.callID)
      }
    return pending().filter((i) => !i.request.tool?.callID || !calls.has(i.request.tool.callID))
  })

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
            <For each={turns()}>
              {(turn, ti) => (
                <TurnView
                  turn={turn}
                  last={ti() === turns().length - 1}
                  busy={busy()}
                  byCall={byCall()}
                  onOpenSession={props.onOpenSession}
                />
              )}
            </For>
          </Show>
          <For each={orphans()}>
            {(i) => <Slip interrupt={i} active inline onOpenSession={(id) => props.onOpenSession(id, false)} />}
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
  turn: Turn
  last: boolean
  busy: boolean
  byCall: Map<string, Interrupt>
  onOpenSession: (id: string, beside: boolean) => void
}) {
  const head = () => props.turn.replies[0]
  const totals = createMemo(() => {
    let tokens = 0
    let cost = 0
    for (const r of props.turn.replies) {
      tokens += r.tokens.input + r.tokens.output + r.tokens.reasoning + r.tokens.cache.read + r.tokens.cache.write
      cost += r.cost
    }
    const first = props.turn.replies[0]?.time.created
    const end = props.turn.replies[props.turn.replies.length - 1]?.time.completed
    return { tokens, cost, ms: first && end ? end - first : 0 }
  })
  const streaming = () => props.last && props.busy
  return (
    <>
      <Show when={props.turn.user}>
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
              <For each={props.turn.replies}>
                {(reply) => (
                  <>
                    <For each={state.parts[reply.id] ?? []}>
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
                    <Show when={reply.error}>{(err) => <ErrorPlate error={err()} sessionID={reply.sessionID} />}</Show>
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

// A session transcript. Messages group into turns (a prompt and the assistant
// steps that answer it); each pending permission or question renders inline
// right after the tool call that raised it. The view sticks to the bottom
// while the reader is there, and offers "Latest" when they have scrolled up.
import { createMemo, createSignal, For, onCleanup, onMount, Show } from "solid-js"
import { api } from "../../api/client"
import type { AssistantMessage, UserMessage } from "../../api/types"
import { fleetState, interruptsFor, type Interrupt } from "../../fleet/derive"
import { state } from "../../store/store"
import { hm, ktok, money } from "../../ui/format"
import { Icon } from "../../ui/icons"
import { writeClipboard } from "../../ui/markdown"
import { Slip } from "../queue/slip"
import { toast } from "../toast"
import { setComposerDraft } from "./composer"
import { replyMarkdown } from "./export"
import { PartView } from "./parts"
import { canRestoreFiles, isReverted, promptText, revertBoundary, revertTo } from "./revert"

/**
 * Scroll a transcript to the previous (-1) or next (+1) message and focus
 * it, so a reader can keep going from the keyboard. From a focused message
 * it steps by one; otherwise from the first message at the top of the view
 * (which is itself the first stop when nothing sits above it).
 */
export function stepMessage(scroller: HTMLElement, offset: -1 | 1) {
  const msgs = [...scroller.querySelectorAll<HTMLElement>(".msg")]
  const focused = msgs.indexOf(document.activeElement as HTMLElement)
  let target: HTMLElement | undefined
  if (focused >= 0) target = msgs[focused + offset]
  else {
    const top = scroller.getBoundingClientRect().top
    // 2px slack for rounding; a message within 24px of the top counts as "here".
    const at = msgs.findIndex((m) => m.getBoundingClientRect().top >= top - 2)
    // Every message starts above the view: the last one is the one being read.
    if (at < 0) target = offset < 0 ? msgs[msgs.length - 1] : undefined
    else {
      const aligned = msgs[at]!.getBoundingClientRect().top - top < 24
      target = offset < 0 ? (msgs[at - 1] ?? msgs[at]) : aligned ? msgs[at + 1] : msgs[at]
    }
  }
  if (!target) return
  target.scrollIntoView({ block: "start" })
  target.focus({ preventScroll: true })
}

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
  /** The first rolled-back turn; it and every later turn render dimmed under a rule. */
  const firstReverted = createMemo(() =>
    revertBoundary(state, props.sessionID) ? turns().find((k) => isReverted(state, props.sessionID, k)) : undefined,
  )

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
    // Content growth (streaming) and scroller shrink (the composer growing,
    // a phone keyboard opening) both keep a reader at the bottom there.
    const ro = new ResizeObserver(() => stuck() && toBottom())
    ro.observe(content)
    ro.observe(scroller)
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
                <>
                  <Show when={key === firstReverted()}>
                    <div class="compacted-rule reverted-rule">Rolled back · the next prompt discards these turns</div>
                  </Show>
                  <TurnView
                    sessionID={props.sessionID}
                    turnKey={key}
                    last={key === turns()[turns().length - 1]}
                    busy={busy()}
                    reverted={!!firstReverted() && key >= firstReverted()!}
                    byCall={byCall()}
                    onOpenSession={props.onOpenSession}
                  />
                </>
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
  reverted: boolean
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

  // Copy reads "Copied" in place for a moment; there is no toast for success.
  const [copied, setCopied] = createSignal(false)
  let copiedTimer: ReturnType<typeof setTimeout> | undefined
  onCleanup(() => clearTimeout(copiedTimer))
  const copyReply = async () => {
    if (!(await writeClipboard(replyMarkdown(state, replies())))) return toast("Could not copy the reply", "error")
    setCopied(true)
    clearTimeout(copiedTimer)
    copiedTimer = setTimeout(() => setCopied(false), 1500)
  }
  const forkAt = async (messageID: string) => {
    try {
      const f = await api.fork(props.sessionID, messageID)
      const m = state.message[messageID] as UserMessage | undefined
      setComposerDraft(f.id, {
        text: promptText(state, messageID),
        agent: m?.agent,
        model: m?.model ? `${m.model.providerID}/${m.model.modelID}` : undefined,
        variant: m?.model?.variant,
      })
      props.onOpenSession(f.id, true)
    } catch (err) {
      toast(`Could not fork: ${err instanceof Error ? err.message : err}`, "error")
    }
  }
  const revertAt = (messageID: string) =>
    revertTo(props.sessionID, messageID).catch((err) =>
      toast(`Could not revert: ${err instanceof Error ? err.message : err}`, "error"),
    )
  return (
    <>
      <Show when={user()}>
        {(u) => (
          <div class="msg msg-user" tabindex="-1" classList={{ reverted: props.reverted }}>
            <div class="msg-label">
              You<span class="mono">{hm(u().time.created)}</span>
              <span class="msg-acts">
                <button
                  class="link-btn"
                  title="A new session with the conversation before this prompt, the prompt in its composer"
                  onClick={() => void forkAt(u().id)}
                >
                  <Icon name="fork" />
                  Fork
                </button>
                <Show when={canRestoreFiles(state, props.sessionID) && !props.reverted}>
                  <button
                    class="link-btn"
                    title="Roll back this prompt and everything after it, files included (Unrevert undoes it)"
                    onClick={() => void revertAt(u().id)}
                  >
                    <Icon name="undo" />
                    Revert
                  </button>
                </Show>
              </span>
            </div>
            {/* A shell turn's prompt is only the server's synthetic note: no empty plate. */}
            <div
              class="body"
              hidden={(state.parts[u().id] ?? []).every((pid) => {
                const p = state.part[pid]
                return !p || (p.type === "text" && !!(p as { synthetic?: boolean }).synthetic)
              })}
            >
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
          <div class="msg msg-asst" tabindex="-1" classList={{ reverted: props.reverted }}>
            <div class="msg-label">
              {h().agent}
              <span class="mono">
                {h().modelID} · {hm(h().time.created)}
              </span>
              <Show when={!streaming()}>
                <span class="msg-acts">
                  <button class="link-btn" title="Copy this reply as Markdown" onClick={() => void copyReply()}>
                    <Icon name="copy" />
                    {copied() ? "Copied" : "Copy"}
                  </button>
                </span>
              </Show>
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
                              {/* A call waiting on the human shows as its slip, not as a "running" row too. */}
                              <Show
                                when={!(p().type === "tool" && props.byCall.has((p() as { callID: string }).callID))}
                              >
                                <PartView part={p()} streaming={streaming()} onOpenSession={props.onOpenSession} />
                              </Show>
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

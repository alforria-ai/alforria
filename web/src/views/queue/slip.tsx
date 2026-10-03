// One waiting item, as an "interrupt slip": a permission to grant or a
// question to answer. The same component renders in the queue and inline at
// the end of a transcript; both act through `act()`, so a slip answered in
// one place is stamped in both.
import { createMemo, createSignal, For, Index, Match, Show, Switch } from "solid-js"
import type { PermissionRequest, QuestionRequest } from "../../api/types"
import { toolTarget } from "../../fleet/derive"
import { displayTitle, projectName } from "../../fleet/fleet"
import type { Interrupt } from "../../fleet/derive"
import { state } from "../../store/store"
import { Diff, parseUnifiedDiff, diffStats } from "../../ui/diff"
import { now, waited } from "../../ui/format"
import { act, stamped, type Verdict } from "./act"

export function slipKind(i: Interrupt) {
  if (i.kind === "question") return "Question"
  const p = i.request.permission
  if (p === "doom_loop") return "Doom loop"
  if (p === "external_directory") return "Outside project"
  return `Permission · ${p}`
}

/** One line describing what is being asked, for collapsed slips and the ledger. */
export function slipSummary(i: Interrupt) {
  if (i.kind === "question") return i.request.questions[0]?.question ?? ""
  const p = i.request
  const meta = p.metadata as Record<string, unknown>
  if (typeof meta.command === "string") return `$ ${meta.command}`
  if (p.permission === "doom_loop") return `${String(meta.tool ?? p.patterns[0] ?? "")}  ×3`
  if (typeof meta.diff === "string") {
    const { add, del } = diffStats(parseUnifiedDiff(meta.diff))
    return `${String(meta.filepath ?? p.patterns.join(", "))}  +${add} −${del}`
  }
  if (typeof meta.url === "string") return meta.url
  return p.patterns.join("  ")
}

function sessionMeta(sessionID: string) {
  const s = state.sessions[sessionID]
  const p = s && state.projects[s.projectID]
  return {
    title: displayTitle(s),
    project: p ? projectName(p) : "",
    agent: s?.agent ?? "",
    model: s?.model?.id ?? "",
  }
}

export function Slip(props: {
  interrupt: Interrupt
  active: boolean
  inline?: boolean
  arriving?: boolean
  onExpand?: () => void
  onOpenSession?: (id: string) => void
  ref?: (el: HTMLDivElement) => void
}) {
  const i = () => props.interrupt
  const stamp = () => stamped[i().id]
  const meta = createMemo(() => sessionMeta(i().sessionID))
  return (
    <div
      ref={props.ref}
      class="slip"
      classList={{
        "is-active": props.active || props.inline,
        inline: props.inline,
        arriving: props.arriving,
        "is-stamped": !!stamp(),
        folding: !!stamp()?.folding,
        folded: !!stamp()?.folding,
      }}
      data-slip={i().id}
      tabindex={props.active || props.inline ? 0 : -1}
      role="group"
      aria-label={`${slipKind(i())} for ${meta().title}`}
      onClick={() => !props.active && !props.inline && props.onExpand?.()}
    >
      <div class="fold-inner">
        <div class="slip-head">
          <span class="lamp waiting" />
          <span class="kind">{slipKind(i())}</span>
          <span class="age">
            {stamp() ? "by you" : `${props.inline ? "waiting " : ""}${waited(now() - i().since)}`}
          </span>
        </div>
        <Show
          when={props.active || props.inline}
          fallback={
            <button class="slip-summary" onClick={() => props.onExpand?.()}>
              <span class={`what ${i().kind === "question" ? "prose" : ""}`}>{slipSummary(i())}</span>
              <span class="who">
                {meta().title}
                {meta().project ? ` · ${meta().project}` : ""}
              </span>
            </button>
          }
        >
          <Show when={!props.inline}>
            <div class="slip-ctx">
              <a
                class="session-link"
                href={`#/focus/${i().sessionID}`}
                onClick={(e) => {
                  e.preventDefault()
                  props.onOpenSession?.(i().sessionID)
                }}
              >
                {meta().title}
              </a>
              <div class="meta">{[meta().project, meta().agent, meta().model].filter(Boolean).join(" · ")}</div>
            </div>
          </Show>
          <Switch>
            <Match when={i().kind === "permission" && i()}>
              <PermissionBody interrupt={i() as Extract<Interrupt, { kind: "permission" }>} />
            </Match>
            <Match when={i().kind === "question" && i()}>
              <QuestionBody interrupt={i() as Extract<Interrupt, { kind: "question" }>} />
            </Match>
          </Switch>
        </Show>
        <Show when={stamp()}>
          {(s) => (
            <div class="verdict-stamp" aria-hidden="true">
              <b>{s().label}</b>
              <span>{s().time} · by you</span>
            </div>
          )}
        </Show>
      </div>
    </div>
  )
}

function alwaysNote(p: PermissionRequest) {
  const a = p.always.filter(Boolean)
  if (!a.length) return null
  if (a.includes("*")) return `every ${p.permission} request`
  return a.join(", ")
}

function PermissionBody(props: { interrupt: Extract<Interrupt, { kind: "permission" }> }) {
  const p = () => props.interrupt.request
  const meta = () => p().metadata as Record<string, unknown>
  const doom = () => p().permission === "doom_loop"
  const [note, setNote] = createSignal<string | null>(null)
  const what = () => `${sessionMeta(p().sessionID).title}: ${slipSummary(props.interrupt)}`
  const send = (v: Verdict, message?: string) => act(props.interrupt, v, { message, what: what() })
  const always = () => alwaysNote(p())
  return (
    <>
      <div class="slip-body">
        <Switch fallback={<div class="plate">{p().patterns.join("\n")}</div>}>
          <Match when={doom()}>
            <p>The agent called {String(meta().tool ?? "a tool")} with identical input 3 times in a row.</p>
            <div class="plate">{doomTarget(meta())}</div>
          </Match>
          <Match when={typeof meta().command === "string"}>
            <Show when={p().permission === "external_directory"}>
              <p>Runs outside the project: {(meta().directories as string[] | undefined)?.join(", ")}</p>
            </Show>
            <div class="plate cmdline">{meta().command as string}</div>
          </Match>
          <Match when={typeof meta().diff === "string"}>
            <Diff rows={parseUnifiedDiff(meta().diff as string)} />
          </Match>
          <Match when={typeof meta().url === "string"}>
            <div class="plate">{meta().url as string}</div>
          </Match>
          <Match when={p().permission === "external_directory"}>
            <p>Reads or writes outside the project.</p>
            <div class="plate">{String(meta().filepath ?? meta().parentDir ?? p().patterns.join("\n"))}</div>
          </Match>
        </Switch>
        <Show when={always()}>
          <div class="pattern-note">
            Always allow remembers <code>{always()}</code> for this session.
          </div>
        </Show>
        <Show when={note() !== null}>
          <input
            class="other-input"
            placeholder="Tell the agent why (optional), then ↵"
            aria-label="Reason for denying"
            value={note()!}
            onInput={(e) => setNote(e.currentTarget.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                e.preventDefault()
                void send("reject", note() || undefined)
              }
              if (e.key === "Escape") setNote(null)
              e.stopPropagation()
            }}
            ref={(el) => setTimeout(() => el.focus())}
          />
        </Show>
      </div>
      <div class="slip-actions">
        <button class="stamp primary" data-verdict="once" onClick={() => send("once")}>
          <kbd>A</kbd>
          {doom() ? "Continue" : "Allow once"}
        </button>
        <button class="stamp" data-verdict="always" onClick={() => send("always")}>
          <kbd>S</kbd>Always
        </button>
        <button
          class="stamp"
          data-verdict="reject"
          onClick={(e) => (e.shiftKey ? setNote("") : send("reject"))}
          title="Shift-click to tell the agent why"
        >
          <kbd>D</kbd>
          {doom() ? "Stop" : "Deny"}
        </button>
      </div>
    </>
  )
}

function doomTarget(meta: Record<string, unknown>) {
  const tool = String(meta.tool ?? "")
  const input = (meta.input ?? {}) as Record<string, unknown>
  const target = toolTarget({ tool, state: { input } } as never)
  return `${tool}  ${target || JSON.stringify(input)}`
}

function QuestionBody(props: { interrupt: Extract<Interrupt, { kind: "question" }> }) {
  const q = (): QuestionRequest => props.interrupt.request
  // answers[i] = selected option labels for question i; custom[i] = free text.
  const [answers, setAnswers] = createSignal<string[][]>(q().questions.map(() => []))
  const [custom, setCustom] = createSignal<string[]>(q().questions.map(() => ""))
  const toggle = (qi: number, label: string, multiple: boolean) =>
    setAnswers((a) =>
      a.map((sel, i) =>
        i !== qi ? sel : multiple ? (sel.includes(label) ? sel.filter((l) => l !== label) : [...sel, label]) : [label],
      ),
    )
  const final = () => answers().map((sel, i) => (custom()[i]?.trim() ? [...sel, custom()[i]!.trim()] : sel))
  const ready = () => final().every((a) => a.length > 0)
  const submit = () => {
    if (!ready()) return false
    void act(props.interrupt, "answer", {
      answers: final(),
      what: `${sessionMeta(q().sessionID).title}: ${final()
        .map((a) => a.join(", "))
        .join(" / ")}`,
    })
    return true
  }
  // Exposed for the queue's keyboard handler (1–9, ↵, X).
  const api = {
    pick: (n: number) => {
      const qi = answers().findIndex((a) => !a.length)
      const at = qi < 0 ? 0 : qi
      const opt = q().questions[at]?.options[n]
      if (opt) toggle(at, opt.label, !!q().questions[at]?.multiple)
    },
    submit,
  }
  return (
    <div ref={(el) => ((el as unknown as { __slip: typeof api }).__slip = api)} class="question-wrap">
      <div class="slip-body">
        <Index each={q().questions}>
          {(question, qi) => (
            <div class="question">
              <p class="q-text">{question().question}</p>
              <div class="options" role={question().multiple ? "group" : "radiogroup"} aria-label={question().header}>
                <For each={question().options}>
                  {(o, oi) => (
                    <button
                      class="option"
                      role={question().multiple ? "checkbox" : "radio"}
                      aria-checked={answers()[qi]?.includes(o.label) ?? false}
                      onClick={() => toggle(qi, o.label, !!question().multiple)}
                    >
                      <kbd class="k">{oi() + 1}</kbd>
                      <b>{o.label}</b>
                      <span>{o.description}</span>
                    </button>
                  )}
                </For>
              </div>
              <Show when={question().custom !== false}>
                <input
                  class="other-input"
                  placeholder="Or type your own answer…"
                  aria-label="Custom answer"
                  value={custom()[qi] ?? ""}
                  onInput={(e) => {
                    const v = e.currentTarget.value
                    setCustom((c) => c.map((x, i) => (i === qi ? v : x)))
                  }}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") {
                      e.preventDefault()
                      submit()
                    }
                    e.stopPropagation()
                  }}
                />
              </Show>
            </div>
          )}
        </Index>
      </div>
      <div class="slip-actions two">
        <button class="stamp primary" data-verdict="answer" disabled={!ready()} onClick={submit}>
          <kbd>↵</kbd>Submit answer
        </button>
        <button
          class="stamp"
          data-verdict="dismiss"
          onClick={() => act(props.interrupt, "dismiss", { what: sessionMeta(q().sessionID).title })}
        >
          <kbd>X</kbd>Dismiss
        </button>
      </div>
    </div>
  )
}

/** Keyboard entry points for a rendered slip element (used by the queue). */
export function slipKeys(el: HTMLElement | null | undefined) {
  return (el?.querySelector(".question-wrap") as unknown as { __slip?: { pick(n: number): void; submit(): boolean } })
    ?.__slip
}

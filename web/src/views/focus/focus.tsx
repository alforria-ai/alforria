// Focus: up to three columns, each a session (transcript / changes / files) or
// a terminal. Columns are keyed components over the shared store, so nothing
// reloads when you switch, split or close.
import { createEffect, For, lazy, Match, onCleanup, Show, Suspense, Switch } from "solid-js"
import { api, ApiError } from "../../api/client"
import { fleetState, interruptsFor, lastAssistant, sessionCost } from "../../fleet/derive"
import { projectName } from "../../fleet/fleet"
import { closeOtherColumns, nav, setActiveColumn, setColumnFile, setColumnTab, type Column } from "../../nav/route"
import { setState, state } from "../../store/store"
import { loadTranscript, releaseTranscript } from "../../sync/sync"
import { ktok, money } from "../../ui/format"
import { Icon } from "../../ui/icons"
import { slipKind } from "../queue/slip"
import { CtxCell } from "../table"
import { toast } from "../toast"
import { Composer } from "./composer"
import { Transcript } from "./transcript"

const Changes = lazy(() => import("./changes"))
const Files = lazy(() => import("./files"))

export function Focus(props: {
  maxColumns: number
  onBack: () => void
  onOpen: (id: string, beside: boolean) => void
  onClose: (i: number) => void
  onTerminal?: (projectID: string) => void
}) {
  return (
    <>
      <div class="pane-head ws-head">
        <button class="back-link" onClick={props.onBack}>
          <Icon name="back" />
          Overview<kbd>Esc</kbd>
        </button>
        <span class="spacer" />
        <span class="count">
          {nav.columns.length} of {props.maxColumns} columns
        </span>
        <Show when={nav.columns.length > 1}>
          <button class="link-btn" onClick={closeOtherColumns}>
            Close others
          </button>
        </Show>
      </div>
      <div class="ws-body focus">
        <For each={nav.columns}>
          {(col, i) => (
            <Switch>
              <Match when={col.kind === "session" && (col as Extract<Column, { kind: "session" }>)}>
                {(c) => (
                  <SessionColumn
                    index={i()}
                    column={c()}
                    onClose={() => props.onClose(i())}
                    onBack={props.onBack}
                    onOpen={props.onOpen}
                    onTerminal={props.onTerminal}
                  />
                )}
              </Match>
              <Match when={col.kind === "terminal" && (col as Extract<Column, { kind: "terminal" }>)}>
                {(c) => <TerminalPlaceholder index={i()} project={c().project} onClose={() => props.onClose(i())} />}
              </Match>
            </Switch>
          )}
        </For>
      </div>
    </>
  )
}

function SessionColumn(props: {
  index: number
  column: Extract<Column, { kind: "session" }>
  onClose: () => void
  onBack: () => void
  onOpen: (id: string, beside: boolean) => void
  onTerminal?: (projectID: string) => void
}) {
  const id = () => props.column.session
  createEffect(() => {
    const sid = id()
    // The fleet snapshot holds the 100 most recent sessions per directory; an
    // older one opened by URL is fetched on demand. A 404 means it was deleted.
    if (!state.sessions[sid])
      api
        .session(sid)
        .then((s) => setState("sessions", s.id, s))
        .catch((err) => {
          if (err instanceof ApiError && err.status === 404) {
            toast("That session no longer exists")
            props.onClose()
          }
        })
    void loadTranscript(sid).catch((err) => toast(`Could not load the transcript: ${err.message}`, "error"))
    onCleanup(() => releaseTranscript(sid))
  })
  // Deleted elsewhere (a `session.deleted` event): close rather than show a husk.
  let seen = false
  createEffect(() => {
    if (state.sessions[id()]) seen = true
    else if (seen) {
      seen = false
      toast("A session you had open was deleted")
      props.onClose()
    }
  })
  const s = () => state.sessions[id()]
  const project = () => (s() ? state.projects[s()!.projectID] : undefined)
  const st = () => fleetState(state, id())
  const waiting = () => interruptsFor(state, id())[0]
  const model = () => {
    const last = lastAssistant(state, id())
    return s()?.model?.id ?? last?.modelID ?? ""
  }
  const tokens = () => {
    const t = s()?.tokens
    return t ? t.input + t.output + t.reasoning + t.cache.read + t.cache.write : 0
  }
  const changeCount = () => s()?.summary?.files ?? state.diffs[id()]?.length ?? 0
  let el!: HTMLElement

  const fork = async () => {
    try {
      const f = await api.fork(id())
      props.onOpen(f.id, true)
    } catch (err) {
      toast(`Could not fork: ${err instanceof Error ? err.message : err}`, "error")
    }
  }
  const compact = async () => {
    const last = lastAssistant(state, id())
    const providerID = s()?.model?.providerID ?? last?.providerID
    const modelID = s()?.model?.id ?? last?.modelID
    if (!providerID || !modelID) return toast("No model to compact with yet", "error")
    try {
      await api.summarize(id(), { providerID, modelID })
    } catch (err) {
      toast(`Could not compact: ${err instanceof Error ? err.message : err}`, "error")
    }
  }

  return (
    <section
      ref={el}
      class="col"
      classList={{ "is-active": nav.active === props.index }}
      data-col={props.index}
      aria-label={s()?.title ?? "Session"}
      onFocusIn={() => setActiveColumn(props.index)}
      onMouseDown={() => setActiveColumn(props.index)}
    >
      <Show when={waiting()}>
        {(w) => (
          <button
            class="col-band"
            onClick={() => {
              const slip = el.querySelector<HTMLElement>(`.transcript [data-slip="${w().id}"]`)
              slip?.scrollIntoView({ block: "center", behavior: "smooth" })
              slip?.focus({ preventScroll: true })
            }}
          >
            <span class="lamp waiting" />
            Waiting on you · {slipKind(w())}
            <span class="spacer" />
            <span class="go">
              Jump
              <Icon name="arrow-down" />
            </span>
          </button>
        )}
      </Show>
      <div class="col-head">
        <div class="l1">
          <button class="icon-btn back-btn" aria-label="Back to sessions" onClick={props.onBack}>
            <Icon name="back" />
          </button>
          <span class={`lamp ${st()}`} title={st()} />
          <h2 title={s()?.title}>{s()?.title || "Untitled session"}</h2>
          <div class="acts">
            <Show when={props.onTerminal && project()}>
              <button
                class="term-btn"
                aria-label={`Open a terminal in ${project() ? projectName(project()!) : ""}`}
                onClick={() => props.onTerminal?.(project()!.id)}
              >
                <Icon name="terminal" />
              </button>
            </Show>
            <button aria-label="Fork session" title="Fork session" onClick={() => void fork()}>
              <Icon name="fork" />
            </button>
            <button
              aria-label="Compact session"
              title="Compact: summarize to free context"
              onClick={() => void compact()}
            >
              <Icon name="compact" />
            </button>
            <button aria-label="Close column" title="Close column" onClick={props.onClose}>
              <Icon name="close" />
            </button>
          </div>
        </div>
        <div class="l2">
          <span class="grow">
            {[project() ? projectName(project()!) : "", s()?.agent ?? lastAssistant(state, id())?.agent, model()]
              .filter(Boolean)
              .join(" · ")}
          </span>
          <CtxCell id={id()} />
          <span>{money(sessionCost(state, id()))}</span>
          <span>{ktok(tokens())} tok</span>
        </div>
        <div class="tabs" role="tablist" aria-label="Session views">
          <button
            role="tab"
            aria-selected={props.column.tab === "transcript"}
            onClick={() => setColumnTab(props.index, "transcript")}
          >
            Transcript
          </button>
          <button
            role="tab"
            aria-selected={props.column.tab === "changes"}
            onClick={() => setColumnTab(props.index, "changes")}
          >
            Changes
            <Show when={changeCount()}>
              <span class="c">{changeCount()}</span>
            </Show>
          </button>
          <button
            role="tab"
            aria-selected={props.column.tab === "files"}
            onClick={() => setColumnTab(props.index, "files")}
          >
            Files
          </button>
        </div>
      </div>
      <Switch>
        <Match when={props.column.tab === "files" && s()}>
          <Suspense fallback={<div class="transcript-loading">Loading files…</div>}>
            <Files
              directory={s()!.directory}
              file={props.column.file}
              onOpen={(path) => setColumnFile(props.index, path)}
            />
          </Suspense>
        </Match>
        <Match when={props.column.tab === "changes"}>
          <Suspense fallback={<div class="transcript-loading">Loading changes…</div>}>
            <Changes sessionID={id()} />
          </Suspense>
        </Match>
        <Match when={true}>
          <Transcript sessionID={id()} onOpenSession={props.onOpen} />
          <Composer sessionID={id()} />
        </Match>
      </Switch>
    </section>
  )
}

function TerminalPlaceholder(props: { index: number; project: string; onClose: () => void }) {
  return (
    <section class="col" classList={{ "is-active": nav.active === props.index }} data-col={props.index}>
      <div class="col-head">
        <div class="l1">
          <Icon name="terminal" />
          <h2>Terminal</h2>
          <div class="acts">
            <button aria-label="Close column" onClick={props.onClose}>
              <Icon name="close" />
            </button>
          </div>
        </div>
      </div>
      <div class="q-empty">
        <h3>Terminal</h3>
        <p>PTY terminals arrive with milestone W4.</p>
      </div>
    </section>
  )
}

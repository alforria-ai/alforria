// The composer: prompt box, @file and /command completion, agent and model
// for the next prompt, send / stop. Drafts are kept per session so switching
// columns never loses text.
import { createMemo, createResource, createSignal, For, onCleanup, Show } from "solid-js"
import { createStore } from "solid-js/store"
import { api, type PromptPart } from "../../api/client"
import type { Agent, Command } from "../../api/types"
import { fleetState, lastAssistant } from "../../fleet/derive"
import { state } from "../../store/store"
import { Icon } from "../../ui/icons"
import { toast } from "../toast"

interface Draft {
  text: string
  agent?: string
  model?: string
  files: string[]
}
const [drafts, setDrafts] = createStore<Record<string, Draft>>({})

const agentCache = new Map<string, Promise<Agent[]>>()
const commandCache = new Map<string, Promise<Command[]>>()
const agentsFor = (dir: string) => {
  if (!agentCache.has(dir))
    agentCache.set(
      dir,
      api.agents(dir).catch(() => []),
    )
  return agentCache.get(dir)!
}
const commandsFor = (dir: string) => {
  if (!commandCache.has(dir))
    commandCache.set(
      dir,
      api.commands(dir).catch(() => []),
    )
  return commandCache.get(dir)!
}

type Pop = { kind: "file" | "command"; items: { value: string; hint: string }[]; sel: number; start: number }

export function Composer(props: { sessionID: string }) {
  let ta!: HTMLTextAreaElement
  const session = () => state.sessions[props.sessionID]
  const dir = () => session()?.directory ?? ""
  const draft = () => drafts[props.sessionID] ?? { text: "", files: [] }
  const setDraft = (d: Partial<Draft>) => setDrafts(props.sessionID, { ...draft(), ...d })

  const [agents] = createResource(dir, agentsFor)
  const primary = createMemo(() => (agents() ?? []).filter((a) => a.mode !== "subagent" && !a.hidden))
  const last = createMemo(() => lastAssistant(state, props.sessionID))
  const agent = () => draft().agent ?? session()?.agent ?? last()?.agent ?? primary()[0]?.name ?? "build"
  const model = () =>
    draft().model ??
    (session()?.model ? `${session()!.model!.providerID}/${session()!.model!.id}` : undefined) ??
    (last() ? `${last()!.providerID}/${last()!.modelID}` : undefined)
  const busy = () => {
    const st = fleetState(state, props.sessionID)
    return st === "working" || st === "retry"
  }

  const [pop, setPop] = createSignal<Pop | null>(null)
  const [modelPicker, setModelPicker] = createSignal(false)
  const [sending, setSending] = createSignal(false)

  let findTimer: ReturnType<typeof setTimeout> | undefined
  onCleanup(() => clearTimeout(findTimer))
  const updatePopover = async () => {
    const caret = ta.selectionStart
    const before = ta.value.slice(0, caret)
    const at = /(^|\s)@([\w./-]*)$/.exec(before)
    const slash = /^\/(\S*)$/.exec(before)
    if (at) {
      const query = at[2]!
      const start = caret - query.length - 1
      clearTimeout(findTimer)
      findTimer = setTimeout(async () => {
        const files = await api.findFiles(dir(), query, 12).catch(() => [])
        if (ta.selectionStart !== caret) return
        setPop(files.length ? { kind: "file", items: files.map((f) => ({ value: f, hint: "" })), sel: 0, start } : null)
      }, 120)
      return
    }
    if (slash) {
      const commands = await commandsFor(dir())
      const items = commands
        .filter((c) => c.name.startsWith(slash[1]!))
        .slice(0, 12)
        .map((c) => ({ value: c.name, hint: c.description ?? "" }))
      setPop(items.length ? { kind: "command", items, sel: 0, start: 0 } : null)
      return
    }
    setPop(null)
  }

  const accept = (i: number) => {
    const p = pop()
    const item = p?.items[i]
    if (!p || !item) return
    const caret = ta.selectionStart
    const insert = p.kind === "file" ? `@${item.value} ` : `/${item.value} `
    const text = ta.value.slice(0, p.start) + insert + ta.value.slice(caret)
    ta.value = text
    const pos = p.start + insert.length
    ta.setSelectionRange(pos, pos)
    setDraft({ text, files: p.kind === "file" ? [...new Set([...draft().files, item.value])] : draft().files })
    setPop(null)
    ta.focus()
    autosize()
  }

  const autosize = () => {
    ta.style.height = "auto"
    ta.style.height = `${Math.min(220, ta.scrollHeight + 2)}px`
  }

  const send = async () => {
    const text = ta.value.trim()
    if (!text || sending()) return
    const m = model()
    const modelRef = m ? { providerID: m.split("/")[0]!, modelID: m.split("/").slice(1).join("/") } : undefined
    setSending(true)
    try {
      const cmd = /^\/(\S+)\s*([\s\S]*)$/.exec(text)
      const commands = cmd ? await commandsFor(dir()) : []
      if (cmd && commands.some((c) => c.name === cmd[1])) {
        await api.command(props.sessionID, { command: cmd[1]!, arguments: cmd[2] ?? "", agent: agent(), model: m })
      } else {
        const parts: PromptPart[] = [{ type: "text", text }]
        for (const f of draft().files)
          if (text.includes(`@${f}`))
            parts.push({
              type: "file",
              mime: "text/plain",
              filename: f,
              url: `file://${dir().replace(/\/$/, "")}/${f}`,
            })
        await api.promptAsync(props.sessionID, { parts, agent: agent(), model: modelRef })
      }
      ta.value = ""
      setDraft({ text: "", files: [] })
      autosize()
    } catch (err) {
      toast(`Could not send: ${err instanceof Error ? err.message : err}`, "error")
    } finally {
      setSending(false)
    }
  }

  const stop = () => api.abort(props.sessionID).catch((err) => toast(`Could not stop: ${err.message}`, "error"))

  const onKey = (e: KeyboardEvent) => {
    const p = pop()
    if (p) {
      if (e.key === "ArrowDown" || e.key === "ArrowUp") {
        e.preventDefault()
        const n = p.items.length
        setPop({ ...p, sel: (p.sel + (e.key === "ArrowDown" ? 1 : -1) + n) % n })
        return
      }
      if (e.key === "Enter" || e.key === "Tab") {
        e.preventDefault()
        return accept(p.sel)
      }
      if (e.key === "Escape") {
        e.preventDefault()
        e.stopPropagation()
        return setPop(null)
      }
    }
    if (e.key === "Enter" && !e.shiftKey && !e.isComposing) {
      e.preventDefault()
      void send()
    }
  }

  const modelName = () => {
    const m = model()
    return m ? (state.models[m]?.name ?? m.split("/").slice(1).join("/")) : "default model"
  }
  const modelList = createMemo(() =>
    Object.entries(state.models)
      .map(([id, info]) => ({ id, name: info.name, provider: id.split("/")[0]! }))
      .sort((a, b) => a.provider.localeCompare(b.provider) || a.name.localeCompare(b.name)),
  )

  return (
    <div class="composer">
      <Show when={pop()}>
        {(p) => (
          <div class="popover" role="listbox">
            <header>{p().kind === "file" ? "Files" : "Commands"}</header>
            <For each={p().items}>
              {(item, i) => (
                <button
                  class={i() === p().sel ? "on" : ""}
                  role="option"
                  aria-selected={i() === p().sel}
                  onMouseDown={(e) => {
                    e.preventDefault()
                    accept(i())
                  }}
                >
                  {p().kind === "command" ? `/${item.value}` : item.value}
                  <span>{item.hint}</span>
                </button>
              )}
            </For>
          </div>
        )}
      </Show>
      <Show when={modelPicker()}>
        <div class="popover" role="listbox" aria-label="Model">
          <header>Model for the next prompt</header>
          <For each={modelList()} fallback={<div class="empty-pop">No models loaded</div>}>
            {(m) => (
              <button
                class={m.id === model() ? "on" : ""}
                onMouseDown={(e) => {
                  e.preventDefault()
                  setDraft({ model: m.id })
                  setModelPicker(false)
                  ta.focus()
                }}
              >
                {m.name}
                <span>{m.provider}</span>
              </button>
            )}
          </For>
        </div>
      </Show>
      <textarea
        ref={ta}
        rows="2"
        value={draft().text}
        placeholder={`Message ${agent()} — @ for files, / for commands`}
        aria-label={`Message ${session()?.title ?? "session"}`}
        onInput={() => {
          setDraft({ text: ta.value })
          autosize()
          void updatePopover()
        }}
        onKeyDown={onKey}
        onBlur={() => setTimeout(() => setPop(null), 120)}
      />
      <div class="row">
        <div class="seg" role="group" aria-label="Agent">
          <For each={primary().length ? primary() : [{ name: "build" } as Agent, { name: "plan" } as Agent]}>
            {(a) => (
              <button
                aria-pressed={agent() === a.name}
                title={a.description}
                onClick={() => setDraft({ agent: a.name })}
              >
                {a.name}
              </button>
            )}
          </For>
        </div>
        <button class="model-btn" title={`Model: ${modelName()}`} onClick={() => setModelPicker((v) => !v)}>
          <span class="m">{modelName()}</span>
          <Icon name="chev-down" />
        </button>
        <span class="spacer" />
        <span class="hint">↵ send · ⇧↵ newline</span>
        <Show
          when={busy() && !draft().text.trim()}
          fallback={
            <button class="send" disabled={sending()} onClick={() => void send()}>
              <Icon name="send" />
              Send
            </button>
          }
        >
          <button class="send stop" onClick={() => void stop()}>
            <Icon name="stop" />
            Stop
          </button>
        </Show>
      </div>
    </div>
  )
}

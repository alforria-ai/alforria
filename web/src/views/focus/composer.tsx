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
import { ModelPicker } from "./model-picker"

interface Attachment {
  name: string
  mime: string
  url: string
}

interface Draft {
  text: string
  agent?: string
  model?: string
  files: string[]
  attachments?: Attachment[]
}

const MAX_ATTACHMENT = 5 * 1024 * 1024
/** Text-like files go as text/plain so the server inlines them like a read. */
function attachmentMime(f: File) {
  if (f.type.startsWith("image/") || f.type === "application/pdf") return f.type
  return "text/plain"
}
function readAttachment(f: File): Promise<Attachment> {
  return new Promise((resolve, reject) => {
    if (f.size > MAX_ATTACHMENT) return reject(new Error(`${f.name} is over 5 MB`))
    const mime = attachmentMime(f)
    const r = new FileReader()
    r.onerror = () => reject(r.error ?? new Error(`Could not read ${f.name}`))
    r.onload = () => {
      // Re-tag the data URL with the mime the server expects.
      const data = String(r.result).replace(/^data:[^;,]*/, `data:${mime}`)
      resolve({ name: f.name || "pasted", mime, url: data })
    }
    r.readAsDataURL(f)
  })
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
    const files = draft().files
    const attachments = draft().attachments ?? []
    const m = model()
    const modelRef = m ? { providerID: m.split("/")[0]!, modelID: m.split("/").slice(1).join("/") } : undefined
    // Clear at once so typing can continue; restore only if the send fails and
    // the box is still empty (never overwrite what was typed meanwhile).
    ta.value = ""
    setDraft({ text: "", files: [], attachments: [] })
    autosize()
    const restore = (err: unknown, what: string) => {
      toast(`Could not ${what}: ${err instanceof Error ? err.message : err}`, "error")
      if (!ta.value.trim()) {
        ta.value = text
        setDraft({ text, files, attachments })
        autosize()
      }
    }
    const cmd = /^\/(\S+)\s*([\s\S]*)$/.exec(text)
    const commands = cmd ? await commandsFor(dir()) : []
    if (cmd && commands.some((c) => c.name === cmd[1])) {
      // The command endpoint answers when the whole turn ends; don't hold the
      // composer for that, the transcript shows progress.
      void api
        .command(props.sessionID, { command: cmd[1]!, arguments: cmd[2] ?? "", agent: agent(), model: m })
        .catch((err) => restore(err, `run /${cmd[1]}`))
      return
    }
    setSending(true)
    try {
      const parts: PromptPart[] = [{ type: "text", text }]
      for (const f of files)
        if (text.includes(`@${f}`))
          parts.push({ type: "file", mime: "text/plain", filename: f, url: `file://${dir().replace(/\/$/, "")}/${f}` })
      for (const a of attachments) parts.push({ type: "file", mime: a.mime, filename: a.name, url: a.url })
      await api.promptAsync(props.sessionID, { parts, agent: agent(), model: modelRef })
    } catch (err) {
      restore(err, "send")
    } finally {
      setSending(false)
    }
  }

  let picker!: HTMLInputElement
  let modelBtn!: HTMLButtonElement
  const attach = async (list: FileList | File[]) => {
    for (const f of Array.from(list))
      try {
        const a = await readAttachment(f)
        setDraft({ attachments: [...(draft().attachments ?? []), a] })
      } catch (err) {
        toast(err instanceof Error ? err.message : String(err), "error")
      }
  }
  const detach = (i: number) => setDraft({ attachments: (draft().attachments ?? []).filter((_, j) => j !== i) })

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
        <ModelPicker
          value={model()}
          anchor={() => modelBtn}
          onPick={(id) => {
            setDraft({ model: id })
            setModelPicker(false)
            ta.focus()
          }}
          onClose={(refocus) => {
            setModelPicker(false)
            if (refocus) modelBtn.focus()
          }}
        />
      </Show>
      <Show when={draft().attachments?.length}>
        <div class="attachments" aria-label="Attachments">
          <For each={draft().attachments}>
            {(a, i) => (
              <span class="attach-chip">
                <Icon name="file" />
                <span class="mono">{a.name}</span>
                <button aria-label={`Remove ${a.name}`} onClick={() => detach(i())}>
                  <Icon name="close" />
                </button>
              </span>
            )}
          </For>
        </div>
      </Show>
      <input
        ref={picker}
        type="file"
        multiple
        hidden
        onChange={(e) => {
          if (e.currentTarget.files) void attach(e.currentTarget.files)
          e.currentTarget.value = ""
        }}
      />
      <textarea
        onPaste={(e) => {
          const files = e.clipboardData?.files
          if (files?.length) {
            e.preventDefault()
            void attach(files)
          }
        }}
        onDrop={(e) => {
          const files = e.dataTransfer?.files
          if (files?.length) {
            e.preventDefault()
            void attach(files)
          }
        }}
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
        <button
          ref={modelBtn}
          class="model-btn"
          title={`Model: ${modelName()}`}
          aria-haspopup="listbox"
          aria-expanded={modelPicker()}
          onClick={() => setModelPicker((v) => !v)}
        >
          <span class="m">{modelName()}</span>
          <Icon name="chev-down" />
        </button>
        <button
          class="icon-btn attach"
          aria-label="Attach files"
          title="Attach files (or paste / drop)"
          onClick={() => picker.click()}
        >
          <Icon name="attach" />
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

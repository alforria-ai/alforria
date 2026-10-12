// The composer: prompt box, @file and /command completion, agent and model
// for the next prompt, send / stop. Drafts are kept per session so switching
// columns never loses text.
import { createMemo, createResource, createSignal, For, onCleanup, Show } from "solid-js"
import { createStore, unwrap } from "solid-js/store"
import { api } from "../../api/client"
import type { Agent, Command } from "../../api/types"
import { lastAssistant } from "../../fleet/derive"
import { state } from "../../store/store"
import { Icon } from "../../ui/icons"
import { toast } from "../toast"
import { addComment, clearComments, comments, commentsText } from "./comments"
import { historyStep, loadDrafts, saveDrafts } from "./drafts"
import { enqueue, followupLine, pause, queue, resume, sendNow, take } from "./followups"
import { ModelPicker, VariantPicker } from "./model-picker"
import {
  attachmentKind,
  commandBody,
  cycleVariant,
  lastUserModel,
  looksBinary,
  MENTION,
  promptBody,
  variantIndex,
  type Attachment,
  type Draft as Body,
} from "./prompt"
import { canRestoreFiles, isBusy, promptText, revertedPrompts, unrevert } from "./revert"

interface Draft {
  text: string
  agent?: string
  model?: string
  /** Picked here; "" is the model's default. Unset follows the session's last prompt. */
  variant?: string
  files: string[]
  attachments?: Attachment[]
}

const MAX_ATTACHMENT = 5 * 1024 * 1024
async function readAttachment(f: File): Promise<Attachment> {
  const name = f.name || "pasted"
  if (f.size > MAX_ATTACHMENT) throw new Error(`${name} is over 5 MB`)
  let kind = attachmentKind(name, f.type)
  if (kind === "maybe-text")
    kind = looksBinary(new Uint8Array(await f.slice(0, 8192).arrayBuffer())) ? undefined : "text"
  if (!kind) throw new Error(`${name} can't be attached: only text, images (PNG, JPEG, GIF, WebP) and PDFs`)
  // Text goes as text/plain so the server inlines it like a read.
  const mime = kind === "text" ? "text/plain" : f.type
  return new Promise((resolve, reject) => {
    const r = new FileReader()
    r.onerror = () => reject(r.error ?? new Error(`Could not read ${f.name}`))
    r.onload = () => {
      // Re-tag the data URL with the mime the server expects.
      const data = String(r.result).replace(/^data:[^;,]*/, `data:${mime}`)
      resolve({ name, mime, url: data })
    }
    r.readAsDataURL(f)
  })
}
const [drafts, setDrafts] = createStore<Record<string, Draft>>(loadDrafts())
let saveTimer: ReturnType<typeof setTimeout> | undefined
const persist = () => {
  clearTimeout(saveTimer)
  saveTimer = setTimeout(() => saveDrafts(unwrap(drafts)), 300)
}
// A reload right after typing still keeps the last keystrokes.
addEventListener("pagehide", () => {
  clearTimeout(saveTimer)
  saveDrafts(unwrap(drafts))
})

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
const variantCache = new Map<string, Promise<Record<string, string[]>>>()
const variantsFor = (dir: string) => {
  if (!variantCache.has(dir))
    variantCache.set(
      dir,
      api
        .providers(dir)
        .then(variantIndex)
        .catch(() => {
          // Not cached: the next column to ask tries again.
          variantCache.delete(dir)
          return {}
        }),
    )
  return variantCache.get(dir)!
}

/** Fill a session's composer: a fork carries its prompt over, with the agent and model it used. */
export function setComposerDraft(
  sessionID: string,
  d: { text: string; agent?: string; model?: string; variant?: string },
) {
  setDrafts(sessionID, (prev) => ({ ...(prev ?? { files: [] }), ...d }))
  persist()
}

/** A draft starting with `!` is a shell command (POST /session/:id/shell), as upstream. */
const shellMode = (text: string) => text.startsWith("!")

type Pop = { kind: "file" | "command"; items: { value: string; hint: string }[]; sel: number; start: number }

export function Composer(props: { sessionID: string }) {
  let ta!: HTMLTextAreaElement
  const session = () => state.sessions[props.sessionID]
  const dir = () => session()?.directory ?? ""
  const draft = () => drafts[props.sessionID] ?? { text: "", files: [] }
  const setDraft = (d: Partial<Draft>) => {
    setDrafts(props.sessionID, { ...draft(), ...d })
    persist()
  }

  const [agents] = createResource(dir, agentsFor)
  const primary = createMemo(() => (agents() ?? []).filter((a) => a.mode !== "subagent" && !a.hidden))
  const last = createMemo(() => lastAssistant(state, props.sessionID))
  const agent = () => draft().agent ?? session()?.agent ?? last()?.agent ?? primary()[0]?.name ?? "build"
  const lastUser = createMemo(() => lastUserModel(state, props.sessionID))
  const model = () =>
    draft().model ??
    (session()?.model ? `${session()!.model!.providerID}/${session()!.model!.id}` : undefined) ??
    (lastUser() ? `${lastUser()!.providerID}/${lastUser()!.modelID}` : undefined) ??
    (last() ? `${last()!.providerID}/${last()!.modelID}` : undefined)
  const [variantIndex] = createResource(dir, variantsFor)
  /** The current model's reasoning variants; empty when it has none (or they're not known yet). */
  const variants = () => variantIndex()?.[model() ?? ""] ?? []
  /**
   * The reasoning variant: the one picked here, else the one the session last
   * used while the model stays the same. Dropped when the model doesn't offer it.
   */
  const variant = () => {
    const m = model()
    const picked = draft().variant
    let v: string | undefined
    if (picked !== undefined) v = picked || undefined
    else {
      const u = lastUser()
      const sm = session()?.model
      if (u && m === `${u.providerID}/${u.modelID}`) v = u.variant
      else if (sm && m === `${sm.providerID}/${sm.id}`) v = sm.variant
    }
    const known = variantIndex()?.[m ?? ""]
    return v && (!variantIndex() || !known || known.includes(v)) ? v : undefined
  }
  /** "" picks the model's default explicitly (so a restored variant doesn't come back). */
  const pickVariant = (v: string | undefined) => setDraft({ variant: v ?? "" })
  const [variantPop, setVariantPop] = createSignal(false)
  // The server's word, not the fleet's: a turn waiting on a permission is
  // still running and can be stopped.
  const busy = () => isBusy(state, props.sessionID)
  const coarse = matchMedia("(pointer: coarse)").matches

  const [pop, setPop] = createSignal<Pop | null>(null)
  const [modelPicker, setModelPicker] = createSignal(false)
  const [sending, setSending] = createSignal(false)

  let findTimer: ReturnType<typeof setTimeout> | undefined
  onCleanup(() => clearTimeout(findTimer))
  const updatePopover = async () => {
    const caret = ta.selectionStart
    const before = ta.value.slice(0, caret)
    const at = MENTION.exec(before)
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

  // ↑/↓ walk this session's earlier prompts. ↑ starts only at the very start
  // of the box (so it still moves the caret within text), ↓ only while
  // walking and on the last line; the draft comes back past the newest.
  const history = createMemo(() =>
    (state.messages[props.sessionID] ?? [])
      .filter((id) => state.message[id]?.role === "user")
      .map((id) => promptText(state, id))
      .filter((t) => t.trim()),
  )
  let hist: number | undefined
  let stash = ""
  const walkHistory = (e: KeyboardEvent) => {
    const up = e.key === "ArrowUp"
    const v = ta.value
    if (up ? ta.selectionStart !== 0 && hist === undefined : v.slice(ta.selectionEnd).includes("\n")) return false
    if (up && v.slice(0, ta.selectionStart).includes("\n")) return false
    const step = historyStep(history(), hist, up ? "up" : "down")
    if (step === "stay") return false
    e.preventDefault()
    if (hist === undefined) stash = v
    hist = step.index
    const text = hist === undefined ? stash : history()[hist]!
    ta.value = text
    setDraft({ text })
    autosize()
    const pos = up ? 0 : text.length
    ta.setSelectionRange(pos, pos)
    return true
  }

  /** `later`: while the session works, hold a plain prompt until the turn ends (⌥↵). */
  const send = async (later = false) => {
    const text = ta.value.trim()
    if (!text || sending() || (shellMode(text) && !text.slice(1).trim())) return
    const files = draft().files
    const attachments = draft().attachments ?? []
    const body: Body = {
      text,
      files,
      attachments,
      directory: dir(),
      agent: agent(),
      model: model(),
      variant: variant(),
    }
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
    hist = undefined
    // Review comments ride along with a plain prompt (queued or sent), then clear.
    const review = comments[props.sessionID] ?? []
    const withReview = () => {
      body.context = commentsText(review) || undefined
      if (review.length) clearComments(props.sessionID)
    }
    if (later && busy() && !shellMode(text) && !text.startsWith("/")) {
      withReview()
      return enqueue(props.sessionID, body)
    }
    resume(props.sessionID)
    if (shellMode(text)) {
      // `!cmd` runs in the session's shell as its own turn; like a command it
      // answers only when it ends, so don't hold the composer.
      const command = text.slice(1).trim()
      const m = promptBody(body).model
      void api
        .shell(props.sessionID, { command, agent: agent(), model: m })
        .catch((err) => restore(err, "run the shell command"))
      return
    }
    const cmd = /^\/(\S+)\s*([\s\S]*)$/.exec(text)
    const commands = cmd ? await commandsFor(dir()) : []
    if (cmd && commands.some((c) => c.name === cmd[1])) {
      // The command endpoint answers when the whole turn ends; don't hold the
      // composer for that, the transcript shows progress.
      void api
        .command(props.sessionID, commandBody(body, cmd[1]!, cmd[2] ?? ""))
        .catch((err) => restore(err, `run /${cmd[1]}`))
      return
    }
    setSending(true)
    try {
      withReview()
      await api.promptAsync(props.sessionID, promptBody(body))
    } catch (err) {
      // The comments come back with the text.
      for (const c of review) addComment(props.sessionID, c)
      restore(err, "send")
    } finally {
      setSending(false)
    }
  }

  let picker!: HTMLInputElement
  let modelBtn!: HTMLButtonElement
  let variantBtn!: HTMLButtonElement
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

  // A reverted session: what was rolled back, its first prompt back into the
  // box, or everything back as it was.
  const rolledBack = createMemo(() => revertedPrompts(state, props.sessionID))
  const [unreverting, setUnreverting] = createSignal(false)
  const restorePrompt = () => {
    const text = rolledBack()[0]?.text ?? ""
    ta.value = text
    setDraft({ text })
    autosize()
    ta.focus()
  }
  const undoRevert = async () => {
    setUnreverting(true)
    try {
      await unrevert(props.sessionID)
    } catch (err) {
      toast(`Could not unrevert: ${err instanceof Error ? err.message : err}`, "error")
    } finally {
      setUnreverting(false)
    }
  }

  // Stopping a turn holds the queue: the next follow-up shouldn't start by itself.
  const stop = () => {
    pause(props.sessionID)
    return api.abort(props.sessionID).catch((err) => toast(`Could not stop: ${err.message}`, "error"))
  }

  const queued = () => queue.items[props.sessionID] ?? []
  /** Back into the box to edit; only into an empty box, so nothing typed is lost. */
  const editQueued = (id: string) => {
    if (ta.value.trim()) return toast("Send or clear the current draft first, then edit the queued one", "error")
    const f = take(props.sessionID, id)
    if (!f) return
    ta.value = f.body.text
    setDraft({ text: f.body.text, files: f.body.files, attachments: f.body.attachments })
    autosize()
    ta.focus()
  }

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
    // ⇧⌘D cycles the reasoning variant, as upstream.
    if (e.shiftKey && (e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "d" && variants().length) {
      e.preventDefault()
      e.stopPropagation()
      return pickVariant(cycleVariant(variants(), variant()))
    }
    if (
      (e.key === "ArrowUp" || e.key === "ArrowDown") &&
      !e.shiftKey &&
      !e.altKey &&
      !e.metaKey &&
      !e.ctrlKey &&
      walkHistory(e)
    )
      return
    // An IME commit (keyCode 229) is not a send; on touch, Enter is a newline and Send sends.
    if (e.key === "Enter" && !e.shiftKey && !e.isComposing && e.keyCode !== 229 && !coarse) {
      e.preventDefault()
      void send(e.altKey)
    }
  }

  const modelName = () => {
    const m = model()
    return m ? (state.models[m]?.name ?? m.split("/").slice(1).join("/")) : "default model"
  }

  return (
    <div class="composer" classList={{ shell: shellMode(draft().text) }}>
      <Show when={rolledBack().length}>
        <div class="revert-dock" role="status">
          <Icon name="undo" />
          <span class="k">Rolled back</span>
          <span class="t">
            {rolledBack().length} {rolledBack().length === 1 ? "turn" : "turns"}
            {canRestoreFiles(state, props.sessionID) ? " and their file changes" : ""}. Sending a prompt discards them.
          </span>
          <span class="spacer" />
          <Show when={rolledBack()[0]?.text}>
            <button class="link-btn" onClick={restorePrompt}>
              Restore prompt
            </button>
          </Show>
          <button class="link-btn" disabled={unreverting()} onClick={() => void undoRevert()}>
            Unrevert
          </button>
        </div>
      </Show>
      <Show when={queued().length}>
        <div class="queue-dock" aria-label="Queued follow-ups">
          <div class="qd-head">
            <span class="k">Queued</span>
            <span class="t">
              {queue.paused[props.sessionID]
                ? "Held after Stop. Send one to resume."
                : busy()
                  ? "Sent in order when this turn ends."
                  : "Sending…"}
            </span>
          </div>
          <For each={queued()}>
            {(f) => (
              <div class="qd-item" classList={{ failed: queue.failed[props.sessionID] === f.id }}>
                <span class="mono n">{queued().indexOf(f) + 1}</span>
                <span class="line">{followupLine(f)}</span>
                <Show when={queue.failed[props.sessionID] === f.id}>
                  <span class="mono">failed</span>
                </Show>
                <button
                  class="link-btn"
                  disabled={!!queue.sending[props.sessionID]}
                  onClick={() => void sendNow(props.sessionID, f.id)}
                >
                  Send now
                </button>
                <button class="link-btn" onClick={() => editQueued(f.id)}>
                  Edit
                </button>
                <button
                  class="icon-btn"
                  aria-label={`Remove “${followupLine(f)}”`}
                  onClick={() => take(props.sessionID, f.id)}
                >
                  <Icon name="close" />
                </button>
              </div>
            )}
          </For>
        </div>
      </Show>
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
            setDraft({ model: id, variant: undefined })
            setModelPicker(false)
            ta.focus()
          }}
          onClose={(refocus) => {
            setModelPicker(false)
            if (refocus) modelBtn.focus()
          }}
        />
      </Show>
      <Show when={variantPop() && variants().length}>
        <VariantPicker
          options={variants()}
          value={variant()}
          anchor={() => variantBtn}
          onPick={(v) => {
            pickVariant(v)
            setVariantPop(false)
            ta.focus()
          }}
          onClose={(refocus) => {
            setVariantPop(false)
            if (refocus) variantBtn.focus()
          }}
        />
      </Show>
      <Show when={comments[props.sessionID]?.length}>
        {(n) => (
          <div class="review-note" role="status">
            <span class="k">Review</span>
            <span class="t">
              {n()} line {n() === 1 ? "comment goes" : "comments go"} with your next prompt
            </span>
            <span class="spacer" />
            <button class="link-btn" onClick={() => clearComments(props.sessionID)}>
              Discard
            </button>
          </div>
        )}
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
          hist = undefined
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
        <Show when={variants().length}>
          <button
            ref={variantBtn}
            class="model-btn variant-btn"
            title="Reasoning variant (⇧⌘D cycles)"
            aria-haspopup="listbox"
            aria-expanded={variantPop()}
            onClick={() => setVariantPop((v) => !v)}
          >
            <span class="m">{variant() ?? "default"}</span>
            <Icon name="chev-down" />
          </button>
        </Show>
        <button
          class="icon-btn attach"
          aria-label="Attach files"
          title="Attach files (or paste / drop)"
          onClick={() => picker.click()}
        >
          <Icon name="attach" />
        </button>
        <span class="spacer" />
        <Show when={shellMode(draft().text)}>
          <span class="shell-tag" title={`Runs in ${dir()}`}>
            Shell
          </span>
        </Show>
        <Show when={!coarse}>
          <span class="hint">
            {shellMode(draft().text) ? "↵ run" : busy() ? "↵ send now · ⌥↵ after this turn" : "↵ send"} · ⇧↵ newline
            {history().length ? " · ↑ earlier" : ""}
          </span>
        </Show>
        <Show when={busy()}>
          <button class="send stop" onClick={() => void stop()}>
            <Icon name="stop" />
            Stop
          </button>
        </Show>
        <Show when={busy() && draft().text.trim() && !shellMode(draft().text) && !draft().text.startsWith("/")}>
          <button class="link-btn queue-btn" title="Send when this turn ends (⌥↵)" onClick={() => void send(true)}>
            Queue
          </button>
        </Show>
        <Show when={!busy() || draft().text.trim()}>
          <button class="send" disabled={sending()} onClick={() => void send()}>
            <Icon name="send" />
            Send
          </button>
        </Show>
      </div>
    </div>
  )
}

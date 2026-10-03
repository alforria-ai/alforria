// ⌘K: jump to any session, run a command, or open a file. Sessions and
// commands match locally; files query the server for the active project.
import { createEffect, createMemo, createSignal, For, onCleanup, onMount, Show } from "solid-js"
import { api } from "../api/client"
import { fleetState } from "../fleet/derive"
import { displayTitle, projectName } from "../fleet/fleet"
import { closeOtherColumns, nav, openSettings } from "../nav/route"
import { state } from "../store/store"
import { Icon } from "../ui/icons"
import { theme, toggleTheme } from "../ui/theme"

export interface PaletteActions {
  openSession(id: string): void
  newSession(projectID: string): void
  openTerminal(projectID: string): void
  openFile(sessionID: string, path: string): void
  showKeys(): void
  close(): void
}

interface Item {
  group: string
  label: string
  meta: string
  lamp?: string
  icon?: "file" | "chev-right" | "plus" | "terminal"
  run: () => void
}

const matches = (q: string, text: string) => q.split(/\s+/).every((w) => text.includes(w))

export function Palette(props: { initial?: string; actions: PaletteActions }) {
  let input!: HTMLInputElement
  let list!: HTMLDivElement
  const [q, setQ] = createSignal(props.initial ?? "")
  const [sel, setSel] = createSignal(0)
  const [files, setFiles] = createSignal<string[]>([])

  const activeSession = () => {
    const col = nav.columns[nav.active]
    return col?.kind === "session" ? col.session : undefined
  }
  const fileDir = () => {
    const sid = activeSession()
    if (sid) return state.sessions[sid]?.directory
    return state.projects[state.projectOrder[0] ?? ""]?.worktree
  }

  let timer: ReturnType<typeof setTimeout> | undefined
  createEffect(() => {
    const query = q().trim()
    clearTimeout(timer)
    if (query.length < 2 || !fileDir()) return setFiles([])
    timer = setTimeout(
      () =>
        api
          .findFiles(fileDir()!, query, 8)
          .then(setFiles)
          .catch(() => setFiles([])),
      120,
    )
  })
  onCleanup(() => clearTimeout(timer))

  const items = createMemo<Item[]>(() => {
    const query = q().toLowerCase().trim()
    const out: Item[] = []
    const sessions = Object.values(state.sessions)
      .filter((s) => !s.time.archived)
      .sort((a, b) => b.time.updated - a.time.updated)
    for (const s of sessions) {
      const p = state.projects[s.projectID]
      const name = p ? projectName(p) : ""
      if (query && !matches(query, `${s.title} ${name}`.toLowerCase())) continue
      out.push({
        group: "Sessions",
        label: displayTitle(s),
        meta: `${name} · ${fleetState(state, s.id)}`,
        lamp: fleetState(state, s.id),
        run: () => props.actions.openSession(s.id),
      })
      if (out.length >= 30) break
    }
    const commands: Omit<Item, "group">[] = [
      ...state.projectOrder.flatMap((id) => {
        const p = state.projects[id]
        if (!p) return []
        return [
          {
            label: `New session in ${projectName(p)}`,
            meta: "N",
            icon: "plus" as const,
            run: () => props.actions.newSession(id),
          },
          {
            label: `Open terminal in ${projectName(p)}`,
            meta: "",
            icon: "terminal" as const,
            run: () => props.actions.openTerminal(id),
          },
        ]
      }),
      { label: `Switch to ${theme() === "dark" ? "light" : "dark"} theme`, meta: "", run: toggleTheme },
      { label: "Open settings", meta: "", run: () => openSettings() },
      { label: "Keyboard shortcuts", meta: "?", run: props.actions.showKeys },
      ...(nav.columns.length > 1 ? [{ label: "Close other columns", meta: "", run: closeOtherColumns }] : []),
    ]
    for (const c of commands)
      if (!query || matches(query, c.label.toLowerCase())) out.push({ group: "Commands", icon: "chev-right", ...c })
    const sid = activeSession() ?? Object.values(state.sessions).find((s) => s.directory === fileDir())?.id
    for (const f of files())
      out.push({
        group: `Files${fileDir() ? ` · ${fileDir()!.split("/").pop()}` : ""}`,
        label: f,
        meta: "",
        icon: "file",
        run: () => sid && props.actions.openFile(sid, f),
      })
    return out
  })

  createEffect(() => {
    items()
    setSel((v) => Math.min(v, Math.max(0, items().length - 1)))
  })
  /** Items grouped for rendering, each keeping its flat index for selection. */
  const sections = createMemo(() => {
    const out: { group: string; items: { item: Item; i: number }[] }[] = []
    items().forEach((item, i) => {
      const last = out[out.length - 1]
      if (last?.group === item.group) last.items.push({ item, i })
      else out.push({ group: item.group, items: [{ item, i }] })
    })
    return out
  })

  const run = (i: number) => {
    const item = items()[i]
    if (!item) return
    props.actions.close()
    item.run()
  }
  const onKey = (e: KeyboardEvent) => {
    const n = items().length
    if (e.key === "ArrowDown" || (e.ctrlKey && e.key === "n")) {
      e.preventDefault()
      setSel((v) => (v + 1) % Math.max(1, n))
    } else if (e.key === "ArrowUp" || (e.ctrlKey && e.key === "p")) {
      e.preventDefault()
      setSel((v) => (v - 1 + n) % Math.max(1, n))
    } else if (e.key === "Enter") {
      e.preventDefault()
      run(sel())
    } else if (e.key === "Escape") {
      e.preventDefault()
      e.stopPropagation()
      props.actions.close()
    }
    queueMicrotask(() => list?.querySelector(".res.on")?.scrollIntoView({ block: "nearest" }))
  }

  onMount(() => {
    input.focus()
    input.setSelectionRange(input.value.length, input.value.length)
  })

  return (
    <div class="palette-scrim" onMouseDown={(e) => e.target === e.currentTarget && props.actions.close()}>
      <div class="palette" role="dialog" aria-modal="true" aria-label="Command palette">
        <input
          ref={input}
          value={q()}
          placeholder="Jump to a session, file or command"
          autocomplete="off"
          spellcheck={false}
          role="combobox"
          aria-expanded="true"
          aria-controls="palResults"
          onInput={(e) => {
            setQ(e.currentTarget.value)
            setSel(0)
          }}
          onKeyDown={onKey}
        />
        <div class="results" id="palResults" role="listbox" ref={list}>
          <Show when={items().length} fallback={<div class="empty">Nothing matches “{q()}”.</div>}>
            <For each={sections()}>
              {(sec) => (
                // A listbox holds groups of options; the section title labels the group.
                <div role="group" aria-label={sec.group}>
                  <h4 role="presentation">{sec.group}</h4>
                  <For each={sec.items}>
                    {({ item, i }) => (
                      <button
                        class="res"
                        classList={{ on: i === sel() }}
                        role="option"
                        aria-selected={i === sel()}
                        onMouseMove={() => setSel(i)}
                        onClick={() => run(i)}
                      >
                        <Show when={item.lamp} fallback={<Icon name={item.icon ?? "chev-right"} />}>
                          <span class={`lamp ${item.lamp}`} />
                        </Show>
                        <span class="t">{item.label}</span>
                        <span class="m">{item.meta}</span>
                      </button>
                    )}
                  </For>
                </div>
              )}
            </For>
          </Show>
        </div>
      </div>
    </div>
  )
}

const GROUPS: [string, [string[], string][]][] = [
  [
    "Anywhere",
    [
      [["⌘K"], "Search sessions, files, commands"],
      [["Q"], "Go to the queue"],
      [["N"], "New session"],
      [["Esc"], "Back (focus → overview, settings → where you were)"],
      [["?"], "This sheet"],
    ],
  ],
  [
    "Queue",
    [
      [["A"], "Allow once / continue"],
      [["S"], "Always allow this pattern"],
      [["D"], "Deny / stop (⇧-click Deny to say why)"],
      [["1", "–", "9"], "Pick an answer"],
      [["↵"], "Submit answer"],
      [["J", "K"], "Next / previous item"],
      [["O"], "Open the session"],
    ],
  ],
  [
    "Sessions",
    [
      [["J", "K"], "Move through rows"],
      [["↵"], "Open in focus"],
      [["⇧", "↵"], "Open beside the current column"],
      [["F"], "Back to focus"],
    ],
  ],
  [
    "Focus",
    [
      [["Alt", "↑", "↓"], "Previous / next session in the active column"],
      [["W"], "Open the oldest waiting session"],
      [["[", "]"], "Previous / next column"],
      [["I"], "Write in the active column"],
    ],
  ],
]

export function KeysSheet(props: { close: () => void }) {
  const Row = (p: { keys: string[]; what: string }) => (
    <>
      <dt>
        <For each={p.keys}>{(k) => <kbd>{k}</kbd>}</For>
      </dt>
      <dd>{p.what}</dd>
    </>
  )
  onMount(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" || e.key === "?") {
        e.preventDefault()
        e.stopPropagation()
        props.close()
      }
    }
    document.addEventListener("keydown", onKey, true)
    onCleanup(() => document.removeEventListener("keydown", onKey, true))
  })
  return (
    <div class="keys-sheet" onMouseDown={(e) => e.target === e.currentTarget && props.close()}>
      <div class="keys" role="dialog" aria-modal="true" aria-label="Keyboard shortcuts">
        <h3>Keys</h3>
        <div class="keys-groups">
          <For each={GROUPS}>
            {([title, rows]) => (
              <section>
                <h4>{title}</h4>
                <dl>
                  <For each={rows}>{([keys, what]) => <Row keys={keys} what={what} />}</For>
                </dl>
              </section>
            )}
          </For>
        </div>
      </div>
    </div>
  )
}

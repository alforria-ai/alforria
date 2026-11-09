// Session history: every session the server keeps, however old, filtered by
// folder and title. The fleet shows only the last few days; this is how an
// older session (or a folder that has dropped off the overview) comes back.
// Opening one puts it back in the session list while it is open, and for
// good once it is used again.
//
// The list is read a page at a time from GET /experimental/session (no
// instance boots) and filtered here, so typing and switching folders answer
// at once while older pages are still arriving.
import { createEffect, createMemo, createSignal, For, Index, onCleanup, Show } from "solid-js"
import type { GlobalSession } from "../api/client"
import { displayTitle } from "../fleet/fleet"
import { bucketed, filterHistory, folderOf, historyFolders, lastUsed, type Folder } from "../fleet/history"
import { state } from "../store/store"
import { eachSessionPage, MAX_SESSIONS } from "../sync/history"
import { tildify } from "../ui/format"
import { Icon } from "../ui/icons"
import { DialogHead, dialogId, Modal } from "../ui/modal"
import { server } from "./server"

/** Rows rendered at a time; more as the selection reaches the end. */
const SHOWN = 100

type Walk = "reading" | "done" | "capped" | "failed"

export function SessionHistory(props: {
  /** The folder to open on; every folder when absent. */
  folder?: Folder
  onOpen: (id: string, beside: boolean) => void
  close: () => void
}) {
  let input!: HTMLInputElement
  let list!: HTMLDivElement
  const title = dialogId()
  const [q, setQ] = createSignal("")
  const [folder, setFolder] = createSignal(props.folder?.key ?? "")
  const [sel, setSel] = createSignal(0)
  const [shown, setShown] = createSignal(SHOWN)
  const [all, setAll] = createSignal<GlobalSession[]>([])
  const [walk, setWalk] = createSignal<Walk>("reading")
  // Headings and dates are as of opening: a ticking clock would re-render the list.
  const opened = Date.now()

  // Root sessions only (a subagent opens from its parent), archived ones left
  // out (they have their own list). Newest first, so recent folders and
  // sessions show at once; older pages fill in behind.
  let open = true
  onCleanup(() => (open = false))
  const seen = new Set<string>()
  eachSessionPage({ roots: true }, (items) => {
    if (!open) return false
    // A session updated mid-walk can show up twice; keep its first copy.
    const fresh = items.filter((s) => !seen.has(s.id) && seen.add(s.id))
    if (fresh.length) setAll((a) => [...a, ...fresh])
  })
    .then((complete) => open && setWalk(complete ? "done" : "capped"))
    .catch((err) => {
      console.warn("session history", err)
      if (open) setWalk("failed")
    })

  const folders = createMemo(() => {
    const out = historyFolders(all())
    // The folder asked for is listed before its sessions arrive.
    const want = props.folder
    if (want && !out.some((f) => f.key === want.key)) out.unshift({ ...want, count: 0 })
    return out
  })
  const current = () => folders().find((f) => f.key === folder())

  const matches = createMemo(() => filterHistory(all(), folder(), q()))
  const rows = createMemo(() => matches().slice(0, shown()))
  const groups = createMemo(() => bucketed(rows(), opened))
  const indexOf = createMemo(() => new Map(rows().map((s, i) => [s.id, i])))

  // A new filter starts at the top.
  createEffect(() => {
    folder()
    q()
    setSel(0)
    setShown(SHOWN)
  })
  createEffect(() => setSel((v) => Math.min(v, Math.max(0, rows().length - 1))))

  const openRow = (i: number, beside: boolean) => {
    const s = rows()[i]
    if (!s) return
    props.close()
    props.onOpen(s.id, beside)
  }
  const step = (d: number) => {
    const n = rows().length
    if (!n) return
    const to = sel() + d
    // Reaching the end of what is rendered renders more, if there is more.
    if (to >= n && matches().length > n) {
      setShown((v) => v + SHOWN)
      return setSel(to)
    }
    setSel(Math.max(0, Math.min(n - 1, to)))
  }
  const onKey = (e: KeyboardEvent) => {
    if (e.key === "ArrowDown" || (e.ctrlKey && e.key === "n")) {
      e.preventDefault()
      step(1)
    } else if (e.key === "ArrowUp" || (e.ctrlKey && e.key === "p")) {
      e.preventDefault()
      step(-1)
    } else if (e.key === "PageDown") {
      e.preventDefault()
      step(10)
    } else if (e.key === "PageUp") {
      e.preventDefault()
      step(-10)
    } else if (e.key === "Enter") {
      e.preventDefault()
      openRow(sel(), e.shiftKey)
    } else return
    queueMicrotask(() => list?.querySelector(".res.on")?.scrollIntoView({ block: "nearest" }))
  }

  const optionId = (i: number) => `hist-opt-${i}`
  /** A listed session's title, as the fleet has it when it is loaded (it may have been renamed). */
  const titleOf = (s: GlobalSession) => displayTitle(state.sessions[s.id] ?? s)
  const meta = (s: GlobalSession) => {
    const parts: string[] = []
    const f = folderOf(s)
    if (!folder()) parts.push(f.name)
    // A session in a subfolder or worktree of its project says where.
    if (s.directory !== f.directory) parts.push(tildify(s.directory, server.home))
    parts.push(lastUsed(s.time.updated, opened))
    return parts.join(" · ")
  }

  const empty = () => {
    if (rows().length) return ""
    if (walk() === "reading") return "Reading sessions…"
    if (walk() === "failed" && !all().length) return "Could not read the session history."
    const where = current() ? ` in ${current()!.name}` : ""
    if (q().trim()) return `No session${where} matches “${q().trim()}”.`
    return `No sessions${where} yet.`
  }
  const status = () => {
    const n = matches().length
    const count = `${n.toLocaleString("en")} session${n === 1 ? "" : "s"}`
    if (walk() === "reading") return `${count} · reading older ones…`
    if (walk() === "capped") return `${count} · the newest ${MAX_SESSIONS.toLocaleString("en")} read`
    if (walk() === "failed") return `${count} · could not read them all`
    return count
  }

  return (
    <Modal class="folder history" labelledBy={title} onClose={props.close} initialFocus={() => input}>
      <DialogHead id={title} title="Session history" onClose={props.close} />
      <div class="palette-input">
        <Icon name="search" />
        <input
          ref={input}
          value={q()}
          placeholder="Search titles"
          aria-label="Search session titles"
          autocomplete="off"
          spellcheck={false}
          role="combobox"
          aria-expanded="true"
          aria-controls="histResults"
          aria-autocomplete="list"
          aria-activedescendant={rows().length ? optionId(sel()) : undefined}
          aria-describedby="histStatus"
          onInput={(e) => setQ(e.currentTarget.value)}
          onKeyDown={onKey}
        />
        <select
          class="select history-folder"
          aria-label="Folder"
          title={current() ? tildify(current()!.directory, server.home) : "Every folder"}
          onChange={(e) => {
            setFolder(e.currentTarget.value)
            input.focus()
          }}
        >
          <option value="" selected={!folder()}>
            Every folder
          </option>
          <For each={folders()}>
            {(f) => (
              <option value={f.key} selected={f.key === folder()}>
                {f.name} — {tildify(f.directory, server.home)}
              </option>
            )}
          </For>
        </select>
      </div>
      <div class="results" id="histResults" role="listbox" aria-label="Sessions" ref={list}>
        {/* Groups by position, so a page landing doesn't rebuild the ones shown. */}
        <Index each={groups()}>
          {(g) => (
            <div role="group" aria-label={g().label}>
              <h4 role="presentation">{g().label}</h4>
              <For each={g().items}>
                {(s) => {
                  const i = () => indexOf().get(s.id) ?? -1
                  return (
                    <button
                      id={optionId(i())}
                      tabindex="-1"
                      class="res"
                      classList={{ on: i() === sel() }}
                      role="option"
                      aria-selected={i() === sel()}
                      title={titleOf(s)}
                      onMouseMove={() => setSel(i())}
                      onMouseDown={(e) => e.shiftKey && e.preventDefault()}
                      onClick={(e) => openRow(i(), e.shiftKey)}
                    >
                      <Icon name="chev-right" />
                      <span class="t">{titleOf(s)}</span>
                      <span class="m">
                        <bdi>{meta(s)}</bdi>
                      </span>
                    </button>
                  )
                }}
              </For>
            </div>
          )}
        </Index>
        <Show when={empty()}>
          <div class="empty">{empty()}</div>
        </Show>
      </div>
      <footer class="folder-foot">
        <div class="where" id="histStatus" role="status">
          <span class="k">{current()?.name ?? "Every folder"}</span>
          <span class="p">{status()}</span>
        </div>
        <Show when={matches().length > rows().length}>
          <button class="link-btn" onClick={() => setShown((v) => v + SHOWN)}>
            Show {Math.min(SHOWN, matches().length - rows().length)} more
          </button>
        </Show>
        <span class="history-keys" aria-hidden="true">
          <kbd>↵</kbd> open <kbd>⇧↵</kbd> beside
        </span>
      </footer>
    </Modal>
  )
}

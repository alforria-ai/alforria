// "Open folder": start a session in any folder on the server. The input is a
// path typed against the server's file system with live completion: the
// folder left of the last slash is listed, the rest filters it. Enter steps
// into the highlighted folder (or starts the session on "Start here"),
// ⌘↵ starts one in whatever the input names.
import { createEffect, createMemo, createSignal, For, on, Show } from "solid-js"
import { api } from "../api/client"
import { places } from "../fleet/fleet"
import { basename, tildify } from "../ui/format"
import { Icon } from "../ui/icons"
import { DialogHead, dialogId, Modal } from "../ui/modal"
import { expand, inputFor, joinPath, matchFolders, parentOf, splitInput } from "./folder-path"
import { server } from "./server"

type Row =
  | { kind: "recent"; path: string; name: string }
  | { kind: "here"; path: string }
  | { kind: "up"; path: string }
  | { kind: "folder"; path: string; name: string }

type Listing = { dir: string; names?: string[]; missing?: boolean }

/** Subfolder names per folder, for the life of the page; null when it can't be read. */
const listings = new Map<string, Promise<string[] | null>>()
function listFolders(dir: string) {
  let p = listings.get(dir)
  if (!p) {
    p = api
      .listFiles(dir, "")
      .then((nodes) => nodes.filter((n) => n.type === "directory").map((n) => n.name))
      .catch(() => null)
    listings.set(dir, p)
    // A failed read may be a folder that doesn't exist yet: try again next time.
    void p.then((names) => {
      if (!names) listings.delete(dir)
    })
  }
  return p
}

const RECENT_MAX = 6

export function OpenFolder(props: { onStart: (directory: string) => Promise<void>; close: () => void }) {
  let input!: HTMLInputElement
  let list!: HTMLDivElement
  const title = dialogId()
  const home = () => server.home
  const [q, setQ] = createSignal(inputFor(home() || "/", home()))
  const [sel, setSel] = createSignal(0)
  const [listing, setListing] = createSignal<Listing>({ dir: "" })
  const [busy, setBusy] = createSignal(false)
  const [error, setError] = createSignal("")

  const parts = createMemo(() => splitInput(q(), home()))
  const show = (path: string) => tildify(path, home())

  // List the folder being typed in; a stale answer for a folder the input
  // has since left is dropped.
  createEffect(
    on(
      () => parts().dir,
      (dir) => {
        if (listing().dir !== dir) setListing({ dir })
        void listFolders(dir).then((names) => {
          if (parts().dir === dir) setListing(names ? { dir, names } : { dir, missing: true })
        })
      },
    ),
  )

  const matches = createMemo(() => {
    const l = listing()
    return l.names && l.dir === parts().dir ? matchFolders(l.names, parts().tail) : []
  })

  /** The folder ⌘↵ starts in: the listed one, or the typed name when it is one of its subfolders. */
  const target = createMemo(() => {
    const { dir, tail } = parts()
    const l = listing()
    if (l.dir !== dir || !l.names) return undefined
    if (!tail) return dir
    return l.names.includes(tail) ? joinPath(dir, tail) : undefined
  })

  const rows = createMemo<{ group: string; rows: Row[] }[]>(() => {
    const { dir, tail } = parts()
    const out: { group: string; rows: Row[] }[] = []
    if (dir === expand("~", home())) {
      const word = tail.toLowerCase()
      const recent = places()
        .filter((p) => !word || p.name.toLowerCase().includes(word))
        .slice(0, RECENT_MAX)
        .map((p): Row => ({ kind: "recent", path: p.directory, name: p.name }))
      if (recent.length) out.push({ group: "Recent", rows: recent })
    }
    const l = listing()
    const here: Row[] = []
    if (!tail && l.names && l.dir === dir) here.push({ kind: "here", path: dir })
    if (!tail && dir !== "/") here.push({ kind: "up", path: parentOf(dir) })
    const folders = matches().map((name): Row => ({ kind: "folder", path: joinPath(dir, name), name }))
    if (here.length || folders.length)
      out.push({ group: `In ${dir === expand("~", home()) ? "~" : basename(dir) || "/"}`, rows: [...here, ...folders] })
    return out
  })
  const flat = createMemo(() => rows().flatMap((g) => g.rows))

  createEffect(() => setSel((v) => Math.min(v, Math.max(0, flat().length - 1))))

  const go = (path: string) => {
    setQ(inputFor(path, home()))
    setSel(0)
    setError("")
    input.focus()
  }

  const start = async (path: string | undefined) => {
    if (busy()) return
    if (!path) return setError(`No folder at ${q().replace(/\/$/, "") || "/"}`)
    setBusy(true)
    setError("")
    try {
      await props.onStart(path)
      props.close()
    } catch (err) {
      setError(`Could not start a session: ${err instanceof Error ? err.message : err}`)
      setBusy(false)
    }
  }

  const act = (row: Row | undefined) => {
    if (!row) return void start(target())
    if (row.kind === "here" || row.kind === "recent") return void start(row.path)
    go(row.path)
  }

  const onKey = (e: KeyboardEvent) => {
    const n = flat().length
    if ((e.metaKey || e.ctrlKey) && e.key === "Enter") {
      e.preventDefault()
      void start(target())
    } else if (e.key === "ArrowDown" || (e.ctrlKey && e.key === "n")) {
      e.preventDefault()
      setSel((v) => (v + 1) % Math.max(1, n))
    } else if (e.key === "ArrowUp" || (e.ctrlKey && e.key === "p")) {
      e.preventDefault()
      setSel((v) => (v - 1 + n) % Math.max(1, n))
    } else if (e.key === "Enter") {
      e.preventDefault()
      act(flat()[sel()])
    } else if (e.key === "Tab" && !e.shiftKey) {
      // Complete into the highlighted folder, like a shell.
      const row = flat()[sel()]
      if (!row || row.kind === "here") return
      e.preventDefault()
      go(row.path)
    }
    queueMicrotask(() => list?.querySelector(".res.on")?.scrollIntoView({ block: "nearest" }))
  }

  const optionId = (i: number) => `fold-opt-${i}`
  const label = (row: Row) => (row.kind === "here" ? "Start a session here" : row.kind === "up" ? ".." : row.name)
  const meta = (row: Row) =>
    row.kind === "recent" || row.kind === "here" ? show(row.path) : row.kind === "up" ? "Parent" : ""
  const icon = (row: Row) =>
    row.kind === "here" || row.kind === "recent" ? "plus" : row.kind === "up" ? "back" : "folder"

  const status = () => {
    const l = listing()
    if (l.dir !== parts().dir) return `Reading ${show(parts().dir)}…`
    if (l.missing) return `Can't open ${show(parts().dir)}: no such folder, or it can't be read.`
    if (!flat().length) return `No folder matching “${parts().tail}” in ${show(parts().dir)}.`
    return ""
  }

  return (
    <Modal
      class="folder"
      labelledBy={title}
      onClose={props.close}
      initialFocus={() => {
        input.setSelectionRange(input.value.length, input.value.length)
        return input
      }}
    >
      <DialogHead id={title} title="Open folder" onClose={props.close} />
      <div class="palette-input">
        <Icon name="folder" />
        <input
          ref={input}
          value={q()}
          placeholder="~/path/to/folder"
          aria-label="Folder path"
          autocomplete="off"
          autocapitalize="off"
          spellcheck={false}
          role="combobox"
          aria-expanded="true"
          aria-controls="foldResults"
          aria-autocomplete="list"
          aria-activedescendant={flat().length ? optionId(sel()) : undefined}
          aria-describedby="foldStatus"
          onInput={(e) => {
            setQ(e.currentTarget.value)
            setSel(0)
            setError("")
          }}
          onKeyDown={onKey}
        />
      </div>
      <div class="results" id="foldResults" role="listbox" aria-label="Folders" ref={list}>
        <For each={rows()}>
          {(g) => (
            <div role="group" aria-label={g.group}>
              <h4 role="presentation">{g.group}</h4>
              <For each={g.rows}>
                {(row) => {
                  const i = () => flat().indexOf(row)
                  return (
                    <button
                      id={optionId(i())}
                      tabindex="-1"
                      class="res"
                      classList={{ on: i() === sel(), here: row.kind === "here" }}
                      role="option"
                      aria-selected={i() === sel()}
                      onMouseMove={() => setSel(i())}
                      onClick={() => act(row)}
                    >
                      <Icon name={icon(row)} />
                      <span class="t">{label(row)}</span>
                      <span class="m">
                        <bdi>{meta(row)}</bdi>
                      </span>
                    </button>
                  )
                }}
              </For>
            </div>
          )}
        </For>
        <Show when={status()}>
          <div class="empty">{status()}</div>
        </Show>
      </div>
      <footer class="folder-foot">
        <div class="where" id="foldStatus" role={error() ? "alert" : undefined}>
          <Show when={!error()} fallback={<span class="err">{error()}</span>}>
            <span class="k">Starts in</span>
            <bdi class="p">{target() ? show(target()!) : "—"}</bdi>
          </Show>
        </div>
        <button
          class="stamp primary"
          disabled={!target() || busy()}
          title="Start a session in this folder (⌘↵)"
          onClick={() => void start(target())}
        >
          {busy() ? "Starting…" : "Start session"}
          <kbd>⌘↵</kbd>
        </button>
      </footer>
    </Modal>
  )
}

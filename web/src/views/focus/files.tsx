// The Files view: the session's project tree (lazy, one level at a time) and a
// read-only viewer. Paths are relative to the project directory.
//
// The server has no file-system watcher: `file.watcher.updated` is emitted
// only by the edit tools (edit, write, apply_patch). So the view refreshes
// from those calls as they complete in the store, which covers the same
// changes without a second event path.
import { createEffect, createMemo, createResource, createSignal, For, on, Show, untrack } from "solid-js"
import { createStore } from "solid-js/store"
import { api } from "../../api/client"
import type { ToolPart } from "../../api/types"
import { state } from "../../store/store"
import { Icon } from "../../ui/icons"
import { toolChanges } from "./edits"

type Node = { name: string; path: string; type: "file" | "directory"; ignored: boolean }

/** Expanded directories, their children and failed listings, per project directory. */
const [trees, setTrees] = createStore<
  Record<string, { open: Record<string, boolean>; children: Record<string, Node[]>; failed: Record<string, string> }>
>({})

/** Files (absolute) the edit tools of loaded sessions under `directory` have changed, newest last. */
function editedFiles(directory: string) {
  const out: string[] = []
  for (const sid of Object.keys(state.loaded)) {
    const dir = state.sessions[sid]?.directory
    if (!dir || (dir !== directory && !dir.startsWith(`${directory}/`))) continue
    for (const mid of state.messages[sid] ?? [])
      for (const pid of state.parts[mid] ?? []) {
        const p = state.part[pid]
        if (p?.type !== "tool") continue
        const st = (p as ToolPart).state as {
          status: string
          input?: Record<string, unknown>
          metadata?: Record<string, unknown>
        }
        if (st.status !== "completed") continue
        for (const c of toolChanges((p as ToolPart).tool, st.input ?? {}, st.metadata ?? {}))
          if (c.file) out.push(`${pid}:${c.file.startsWith("/") ? c.file : `${dir}/${c.file}`}`)
      }
  }
  return out
}

export default function Files(props: { directory: string; file?: string; onOpen: (path: string) => void }) {
  const tree = () => trees[props.directory] ?? { open: {}, children: {}, failed: {} }
  const setTree = (
    f: (t: { open: Record<string, boolean>; children: Record<string, Node[]>; failed: Record<string, string> }) => void,
  ) =>
    setTrees(props.directory, (t) => {
      const next = { open: { ...t?.open }, children: { ...t?.children }, failed: { ...t?.failed } }
      f(next)
      return next
    })
  // A failed listing is not cached: it shows an error with Retry and loads again on the next open.
  const load = async (path: string, force = false) => {
    if (!force && tree().children[path]) return
    try {
      const list = await api.listFiles(props.directory, path)
      const sorted = [...list].sort((a, b) =>
        a.type === b.type ? a.name.localeCompare(b.name) : a.type === "directory" ? -1 : 1,
      )
      setTree((t) => {
        t.children[path] = sorted
        delete t.failed[path]
      })
    } catch (err) {
      setTree((t) => {
        if (!force) delete t.children[path]
        t.failed[path] = err instanceof Error ? err.message : String(err)
      })
    }
  }
  const toggle = (path: string) => {
    const open = !tree().open[path]
    setTree((t) => {
      t.open[path] = open
    })
    if (open) void load(path)
  }
  createEffect(() => void load(""))
  // Reveal the selected file when it changes (not when a folder is collapsed
  // afterwards): open every ancestor directory.
  createEffect(
    on(
      () => props.file,
      (f) => {
        if (!f) return
        const parts = f.split("/")
        untrack(() => {
          for (let i = 1; i < parts.length; i++) {
            const dir = parts.slice(0, i).join("/")
            if (!tree().open[dir]) toggle(dir)
          }
        })
      },
    ),
  )

  // Refresh what an edit touched: its folder's listing (if loaded) and the open file.
  const [revision, setRevision] = createSignal(0)
  const edits = createMemo(() => editedFiles(props.directory), [], {
    equals: (a, b) => a.length === b.length && a.every((x, i) => x === b[i]),
  })
  let seen: Set<string> | undefined
  createEffect(() => {
    const now = edits()
    const first = !seen
    const fresh = now.filter((e) => !seen?.has(e))
    seen = new Set(now)
    if (first) return
    const root = props.directory.replace(/\/$/, "")
    for (const e of fresh) {
      const abs = e.slice(e.indexOf(":") + 1)
      if (!abs.startsWith(`${root}/`)) continue
      const rel = abs.slice(root.length + 1)
      const parent = rel.includes("/") ? rel.slice(0, rel.lastIndexOf("/")) : ""
      if (untrack(() => tree().children[parent])) void load(parent, true)
      if (rel === untrack(() => props.file)) setRevision((r) => r + 1)
    }
  })

  const Rows = (p: { path: string; depth: number }) => (
    <>
      <Show when={tree().failed[p.path]}>
        {(msg) => (
          <div class="tree-error" style={{ "padding-left": `${12 + p.depth * 12}px` }} title={msg()}>
            Could not list this folder
            <button class="link-btn" onClick={() => void load(p.path, true)}>
              Retry
            </button>
          </div>
        )}
      </Show>
      <For each={tree().children[p.path] ?? []}>
        {(n) => (
          <>
            <button
              classList={{ on: n.path === props.file, ignored: n.ignored }}
              style={{ "padding-left": `${12 + p.depth * 12}px` }}
              aria-expanded={n.type === "directory" ? !!tree().open[n.path] : undefined}
              onClick={() => (n.type === "directory" ? toggle(n.path) : props.onOpen(n.path))}
            >
              <Icon name={n.type === "directory" ? "folder" : "file"} />
              {n.name}
            </button>
            <Show when={n.type === "directory" && tree().open[n.path]}>
              <Rows path={n.path} depth={p.depth + 1} />
            </Show>
          </>
        )}
      </For>
    </>
  )

  return (
    <div class="files">
      <nav class="tree" aria-label="Project files">
        <Rows path="" depth={0} />
      </nav>
      <Show
        when={props.file}
        fallback={
          <div class="q-empty">
            <h3>Files</h3>
            <p>Pick a file to read it here.</p>
          </div>
        }
      >
        {(f) => <Viewer directory={props.directory} path={f()} revision={revision()} />}
      </Show>
    </div>
  )
}

const MAX_LINES = 5000

function Viewer(props: { directory: string; path: string; revision: number }) {
  const [content, { refetch }] = createResource(
    () => [props.directory, props.path, props.revision] as const,
    ([dir, path]) => api.readFile(dir, path),
  )
  const [wrap, setWrap] = createSignal(false)
  const lines = () => {
    const c = content()
    if (!c || c.type !== "text") return []
    return c.content.split("\n").slice(0, MAX_LINES)
  }
  return (
    <div class="viewer">
      <div class="viewer-head">
        <span class="mono">{props.path}</span>
        <span class="spacer" />
        <button class="link-btn" aria-pressed={wrap()} onClick={() => setWrap((v) => !v)}>
          Wrap
        </button>
      </div>
      <Show when={!content.loading} fallback={<div class="transcript-loading">Loading…</div>}>
        <Show
          when={!content.error}
          fallback={
            <div class="q-empty">
              <h3>Could not read this file</h3>
              <p>{content.error instanceof Error ? content.error.message : String(content.error)}</p>
              <button class="link-btn" onClick={() => void refetch()}>
                Retry
              </button>
            </div>
          }
        >
          <Show
            when={content()?.type === "text"}
            fallback={
              <div class="q-empty">
                <h3>Binary file</h3>
                <p>{content()?.mimeType ?? "Not shown"}</p>
              </div>
            }
          >
            <div class="code" classList={{ wrap: wrap() }} tabindex="0" aria-label={props.path}>
              <For each={lines()}>
                {(l, i) => (
                  <div class="ln">
                    <span>{i() + 1}</span>
                    <span>{l}</span>
                  </div>
                )}
              </For>
            </div>
          </Show>
        </Show>
      </Show>
    </div>
  )
}

// The Files view: the session's project tree (lazy, one level at a time) and a
// read-only viewer. Paths are relative to the project directory.
import { createEffect, createResource, createSignal, For, Show } from "solid-js"
import { createStore } from "solid-js/store"
import { api } from "../../api/client"
import { Icon } from "../../ui/icons"

type Node = { name: string; path: string; type: "file" | "directory"; ignored: boolean }

/** Expanded directories and their children, per project directory. */
const [trees, setTrees] = createStore<
  Record<string, { open: Record<string, boolean>; children: Record<string, Node[]> }>
>({})

export default function Files(props: { directory: string; file?: string; onOpen: (path: string) => void }) {
  const tree = () => trees[props.directory] ?? { open: {}, children: {} }
  const load = async (path: string) => {
    if (tree().children[path]) return
    const list = await api.listFiles(props.directory, path).catch(() => [])
    const sorted = [...list].sort((a, b) =>
      a.type === b.type ? a.name.localeCompare(b.name) : a.type === "directory" ? -1 : 1,
    )
    setTrees(props.directory, (t) => ({ open: t?.open ?? {}, children: { ...t?.children, [path]: sorted } }))
  }
  const toggle = (path: string) => {
    const open = !tree().open[path]
    setTrees(props.directory, (t) => ({ children: t?.children ?? {}, open: { ...t?.open, [path]: open } }))
    if (open) void load(path)
  }
  createEffect(() => void load(""))
  // Reveal the selected file: open every ancestor directory.
  createEffect(() => {
    const f = props.file
    if (!f) return
    const parts = f.split("/")
    for (let i = 1; i < parts.length; i++) {
      const dir = parts.slice(0, i).join("/")
      if (!tree().open[dir]) toggle(dir)
    }
  })

  const Rows = (p: { path: string; depth: number }) => (
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
        {(f) => <Viewer directory={props.directory} path={f()} />}
      </Show>
    </div>
  )
}

const MAX_LINES = 5000

function Viewer(props: { directory: string; path: string }) {
  const [content] = createResource(
    () => [props.directory, props.path] as const,
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
    </div>
  )
}

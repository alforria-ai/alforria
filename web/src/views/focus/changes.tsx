// The Changes view: what this session changed, file by file, with revert.
// Backed by GET /session/{id}/diff and kept fresh by `session.diff` events.
import { createEffect, createMemo, createSignal, For, Show } from "solid-js"
import { api } from "../../api/client"
import type { FileDiff, ToolPart } from "../../api/types"
import { setState, state } from "../../store/store"
import { Diff, parseUnifiedDiff } from "../../ui/diff"
import { Icon } from "../../ui/icons"
import { toast } from "../toast"

export default function Changes(props: { sessionID: string }) {
  const [selected, setSelected] = createSignal(0)
  const [confirm, setConfirm] = createSignal(false)
  const [loading, setLoading] = createSignal(true)

  createEffect(() => {
    const id = props.sessionID
    setLoading(true)
    api
      .diff(id)
      .then((d) => setState("diffs", id, d))
      .catch((err) => toast(`Could not load changes: ${err.message}`, "error"))
      .finally(() => setLoading(false))
  })

  // The server's snapshot diff is the authority; when it is empty (alforria
  // does not compute it yet), rebuild the picture from the session's own
  // edit/write/apply_patch calls.
  const fromServer = () => state.diffs[props.sessionID] ?? []
  const fromTools = createMemo(() => toolDiffs(props.sessionID))
  const derived = () => !fromServer().length && fromTools().length > 0
  const files = createMemo(() => (fromServer().length ? fromServer() : fromTools()))
  const totals = createMemo(() =>
    files().reduce((a, f) => ({ add: a.add + f.additions, del: a.del + f.deletions }), { add: 0, del: 0 }),
  )
  const current = () => files()[Math.min(selected(), files().length - 1)]
  const rows = createMemo(() => (current()?.patch ? parseUnifiedDiff(current()!.patch) : []))

  const revert = async () => {
    // Revert to before this session's first prompt: the earliest user message.
    const first = (state.messages[props.sessionID] ?? []).map((m) => state.message[m]).find((m) => m?.role === "user")
    if (!first) return toast("Nothing to revert to yet", "error")
    try {
      await api.revert(props.sessionID, first.id)
      setConfirm(false)
      toast("Reverted the session's file changes")
    } catch (err) {
      toast(`Could not revert: ${err instanceof Error ? err.message : err}`, "error")
    }
  }
  return (
    <div class="changes">
      <Show
        when={files().length}
        fallback={
          <div class="q-empty">
            <h3>{loading() ? "Loading…" : "No changes yet"}</h3>
            <p>File edits from this session are listed here, with revert.</p>
          </div>
        }
      >
        <div>
          <div class="change-bar">
            <span>
              {files().length} files · <span class="add-n">+{totals().add}</span>{" "}
              <span class="del-n">−{totals().del}</span>
              {derived() ? " · from this session's edits" : ""}
            </span>
            <span class="spacer" />
            <Show
              when={confirm()}
              fallback={
                <button class="link-btn" onClick={() => setConfirm(true)}>
                  <Icon name="undo" />
                  Revert all
                </button>
              }
            >
              <span>Revert {files().length} files to before this session?</span>
              <button class="link-btn" onClick={() => void revert()}>
                Revert
              </button>
              <button class="link-btn" onClick={() => setConfirm(false)}>
                Cancel
              </button>
            </Show>
          </div>
          <div class="change-list" role="listbox" aria-label="Changed files">
            <For each={files()}>
              {(f, i) => (
                <button role="option" aria-selected={i() === selected()} onClick={() => setSelected(i())}>
                  <span>{f.path}</span>
                  <span class="add">+{f.additions}</span>
                  <span class="del">−{f.deletions}</span>
                </button>
              )}
            </For>
          </div>
        </div>
        <div class="change-diff">
          <Diff rows={rows()} />
        </div>
      </Show>
    </div>
  )
}

/** Per-file changes from the session's completed edit tools, in call order. */
function toolDiffs(sessionID: string): FileDiff[] {
  const root = state.sessions[sessionID]?.directory ?? ""
  const rel = (p: string) => (root && p.startsWith(`${root}/`) ? p.slice(root.length + 1) : p)
  const byFile = new Map<string, FileDiff>()
  for (const mid of state.messages[sessionID] ?? [])
    for (const pid of state.parts[mid] ?? []) {
      const part = state.part[pid]
      if (part?.type !== "tool") continue
      const tool = part as ToolPart
      const st = tool.state as { status: string; input?: Record<string, unknown>; metadata?: Record<string, unknown> }
      if (st.status !== "completed" || !["edit", "write", "apply_patch"].includes(tool.tool)) continue
      const meta = st.metadata ?? {}
      const path = rel(String(meta.filepath ?? st.input?.filePath ?? ""))
      if (!path) continue
      let patch = typeof meta.diff === "string" ? meta.diff : ""
      if (!patch && tool.tool === "write" && typeof st.input?.content === "string") {
        const lines = st.input.content.replace(/\n$/, "").split("\n")
        patch = `@@ -0,0 +1,${lines.length} @@\n${lines.map((l) => `+${l}`).join("\n")}`
      }
      const rows = parseUnifiedDiff(patch)
      const add = rows.filter((r) => r.kind === "add").length
      const del = rows.filter((r) => r.kind === "del").length
      const prev = byFile.get(path)
      byFile.set(path, {
        path,
        status: prev ? prev.status : meta.exists === false ? "added" : "modified",
        additions: (prev?.additions ?? 0) + add,
        deletions: (prev?.deletions ?? 0) + del,
        patch: prev ? `${prev.patch}\n${patch}` : patch,
      })
    }
  return [...byFile.values()]
}

// The Changes view: what changed, file by file. As in TS, a turn's diff is
// its user message's `summary.diffs` (snapshots around the turn's steps), so
// the view walks turns, newest first, and reverts to before one. A git
// project can also show the working tree (GET /vcs/diff), which includes
// edits made outside this session. A project without snapshots (not git)
// falls back to this session's own edit/write/apply_patch calls.
import { createEffect, createMemo, createResource, createSignal, For, Show } from "solid-js"
import { api } from "../../api/client"
import type { ToolPart } from "../../api/types"
import { turnDiffs } from "../../fleet/derive"
import { state } from "../../store/store"
import { Diff, parseUnifiedDiff } from "../../ui/diff"
import { Icon } from "../../ui/icons"
import { toast } from "../toast"

interface Change {
  file: string
  patch: string
  additions: number
  deletions: number
}

/** `turn:<userMessageID>`, `tree` or `edits`. */
type Source = string

const excerpt = (text: string) => {
  const line = text.trim().split("\n")[0] ?? ""
  return line.length > 48 ? `${line.slice(0, 47)}…` : line
}

export default function Changes(props: { sessionID: string }) {
  const [picked, setPicked] = createSignal<Source | null>(null)
  const [selected, setSelected] = createSignal(0)
  const [confirm, setConfirm] = createSignal(false)

  const session = () => state.sessions[props.sessionID]
  const turns = createMemo(() => turnDiffs(state, props.sessionID))
  const edits = createMemo(() => toolDiffs(props.sessionID))
  const isGit = () => state.projects[session()?.projectID ?? ""]?.vcs === "git"
  /** Turn number (1-based, oldest first) and prompt excerpt for a user message. */
  const turnLabel = (messageID: string) => {
    const users = (state.messages[props.sessionID] ?? []).filter((m) => state.message[m]?.role === "user")
    const n = users.indexOf(messageID) + 1
    const text = (state.parts[messageID] ?? [])
      .map((p) => state.part[p])
      .find((p) => p?.type === "text" && !(p as { synthetic?: boolean }).synthetic) as { text?: string } | undefined
    return `Turn ${n}${text?.text ? ` · ${excerpt(text.text)}` : ""}`
  }

  // Default: the newest turn with changes; else this session's edits; else the tree.
  const source = (): Source => {
    const p = picked()
    if (p && (p === "tree" || p === "edits" || turns().some((t) => `turn:${t.message.id}` === p))) return p
    if (turns()[0]) return `turn:${turns()[0]!.message.id}`
    if (edits().length) return "edits"
    return isGit() ? "tree" : "edits"
  }

  const [tree, { refetch: refetchTree }] = createResource(
    () => (source() === "tree" && session()?.directory) || false,
    (dir) =>
      api.vcsDiff(dir).catch((err) => {
        toast(`Could not load the working tree: ${err.message}`, "error")
        return []
      }),
  )
  // The tree moves as this session works; refresh when it settles.
  createEffect(() => {
    const busy = state.status[props.sessionID]?.type
    if (busy === "idle" && source() === "tree") void refetchTree()
  })

  const files = createMemo<Change[]>(() => {
    const src = source()
    if (src === "edits") return edits()
    if (src === "tree")
      return (tree() ?? []).map((d) => ({
        file: d.file,
        patch: d.patch ?? "",
        additions: d.additions,
        deletions: d.deletions,
      }))
    const turn = turns().find((t) => `turn:${t.message.id}` === src)
    return (turn?.diffs ?? []).map((d) => ({
      file: d.file ?? "",
      patch: d.patch ?? "",
      additions: d.additions,
      deletions: d.deletions,
    }))
  })
  createEffect(() => {
    source()
    setSelected(0)
    setConfirm(false)
  })
  const totals = createMemo(() =>
    files().reduce((a, f) => ({ add: a.add + f.additions, del: a.del + f.deletions }), { add: 0, del: 0 }),
  )
  const current = () => files()[Math.min(selected(), files().length - 1)]
  const rows = createMemo(() => (current()?.patch ? parseUnifiedDiff(current()!.patch) : []))

  /** Revert target: before the picked turn, or before the session's first prompt. */
  const revertTo = () => {
    const src = source()
    if (src.startsWith("turn:")) return src.slice(5)
    if (src === "edits") return (state.messages[props.sessionID] ?? []).find((m) => state.message[m]?.role === "user")
  }
  const revert = async () => {
    const to = revertTo()
    if (!to) return toast("Nothing to revert to yet", "error")
    try {
      await api.revert(props.sessionID, to)
      setConfirm(false)
      toast("Reverted the session's file changes")
    } catch (err) {
      toast(`Could not revert: ${err instanceof Error ? err.message : err}`, "error")
    }
  }
  const revertQuestion = () =>
    source() === "edits"
      ? `Restore ${files().length} files to before this session?`
      : `Restore files to before this turn? Later turns are undone too.`

  const empty = () => {
    if (source() === "tree") return tree.loading ? "Loading…" : "The working tree is clean"
    return "No changes yet"
  }

  return (
    <div class="changes">
      <div>
        <div class="change-bar">
          <select
            class="select change-source"
            aria-label="Show changes from"
            value={source()}
            onChange={(e) => setPicked(e.currentTarget.value)}
          >
            <For each={turns()}>{(t) => <option value={`turn:${t.message.id}`}>{turnLabel(t.message.id)}</option>}</For>
            <Show when={!turns().length && edits().length}>
              <option value="edits">This session's edits</option>
            </Show>
            <Show when={isGit()}>
              <option value="tree">Working tree (uncommitted)</option>
            </Show>
          </select>
          <Show when={files().length}>
            <span class="mono">
              {files().length} {files().length === 1 ? "file" : "files"} · <span class="add-n">+{totals().add}</span>{" "}
              <span class="del-n">−{totals().del}</span>
            </span>
          </Show>
          <span class="spacer" />
          <Show when={source() === "tree"}>
            <button class="link-btn" onClick={() => void refetchTree()}>
              Refresh
            </button>
          </Show>
          <Show when={source() !== "tree" && files().length}>
            <Show
              when={confirm()}
              fallback={
                <button class="link-btn" onClick={() => setConfirm(true)}>
                  <Icon name="undo" />
                  {source() === "edits" ? "Revert all" : "Revert this turn"}
                </button>
              }
            >
              <span role="alert">{revertQuestion()}</span>
              <button class="link-btn" onClick={() => void revert()}>
                Revert
              </button>
              <button class="link-btn" onClick={() => setConfirm(false)}>
                Cancel
              </button>
            </Show>
          </Show>
        </div>
        <Show when={files().length}>
          <div class="change-list" role="listbox" aria-label="Changed files">
            <For each={files()}>
              {(f, i) => (
                <button role="option" aria-selected={i() === selected()} onClick={() => setSelected(i())}>
                  <span>{f.file}</span>
                  <span class="add">+{f.additions}</span>
                  <span class="del">−{f.deletions}</span>
                </button>
              )}
            </For>
          </div>
        </Show>
      </div>
      <Show
        when={files().length}
        fallback={
          <div class="q-empty">
            <h3>{empty()}</h3>
            <p>
              {isGit()
                ? "Each turn's file changes are listed here once its steps finish, with revert."
                : "This project isn't a git repository, so changes come from this session's edit calls."}
            </p>
          </div>
        }
      >
        <div class="change-diff">
          <Diff rows={rows()} />
        </div>
      </Show>
    </div>
  )
}

/** Per-file changes from the session's completed edit tools, in call order. */
function toolDiffs(sessionID: string): Change[] {
  const root = state.sessions[sessionID]?.directory ?? ""
  const rel = (p: string) => (root && p.startsWith(`${root}/`) ? p.slice(root.length + 1) : p)
  const byFile = new Map<string, Change>()
  for (const mid of state.messages[sessionID] ?? [])
    for (const pid of state.parts[mid] ?? []) {
      const part = state.part[pid]
      if (part?.type !== "tool") continue
      const tool = part as ToolPart
      const st = tool.state as { status: string; input?: Record<string, unknown>; metadata?: Record<string, unknown> }
      if (st.status !== "completed" || !["edit", "write", "apply_patch"].includes(tool.tool)) continue
      const meta = st.metadata ?? {}
      const file = rel(String(meta.filepath ?? st.input?.filePath ?? ""))
      if (!file) continue
      let patch = typeof meta.diff === "string" ? meta.diff : ""
      if (!patch && tool.tool === "write" && typeof st.input?.content === "string") {
        const lines = st.input.content.replace(/\n$/, "").split("\n")
        patch = `@@ -0,0 +1,${lines.length} @@\n${lines.map((l) => `+${l}`).join("\n")}`
      }
      const rows = parseUnifiedDiff(patch)
      const add = rows.filter((r) => r.kind === "add").length
      const del = rows.filter((r) => r.kind === "del").length
      const prev = byFile.get(file)
      byFile.set(file, {
        file,
        additions: (prev?.additions ?? 0) + add,
        deletions: (prev?.deletions ?? 0) + del,
        patch: prev ? `${prev.patch}\n${patch}` : patch,
      })
    }
  return [...byFile.values()]
}

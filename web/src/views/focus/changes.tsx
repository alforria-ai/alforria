// The Changes view: what changed, file by file. As in TS, a turn's diff is
// its user message's `summary.diffs` (snapshots around the turn's steps), so
// the view walks turns, newest first, and reverts to before one. A git
// project can also show the working tree (GET /vcs/diff), which includes
// edits made outside this session. A project without snapshots (not git)
// falls back to this session's own edit/write/apply_patch calls, and offers
// no revert: the server could not put the files back, and the next prompt
// would still discard the rolled-back turns.
import { createEffect, createMemo, createResource, createSignal, For, Show } from "solid-js"
import { api } from "../../api/client"
import type { ToolPart } from "../../api/types"
import { turnDiffs } from "../../fleet/derive"
import { state } from "../../store/store"
import { Diff, parseUnifiedDiff, type LineRow } from "../../ui/diff"
import { Icon } from "../../ui/icons"
import { toast } from "../toast"
import { addComment, comments, commentsAt } from "./comments"
import { toolChanges } from "./edits"
import { CommentBox, CommentNotes } from "./line-comments"
import { canRestoreFiles, isBusy, isReverted, revertTo, unrevert } from "./revert"

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
  const isGit = () => canRestoreFiles(state, props.sessionID)
  /** Turn number (1-based, oldest first) and prompt excerpt for a user message. */
  const turnLabel = (messageID: string) => {
    const users = (state.messages[props.sessionID] ?? []).filter((m) => state.message[m]?.role === "user")
    const n = users.indexOf(messageID) + 1
    const text = (state.parts[messageID] ?? [])
      .map((p) => state.part[p])
      .find((p) => p?.type === "text" && !(p as { synthetic?: boolean }).synthetic) as { text?: string } | undefined
    return `Turn ${n}${text?.text ? ` · ${excerpt(text.text)}` : ""}`
  }

  /** The sources the picker renders; a pick outside them (a turn that left) falls back. */
  const options = createMemo<Source[]>(() => [
    ...turns().map((t) => `turn:${t.message.id}`),
    ...(!turns().length && edits().length ? ["edits"] : []),
    ...(isGit() ? ["tree"] : []),
  ])
  // Default: the newest turn with changes; else this session's edits; else the tree.
  const source = (): Source => {
    const p = picked()
    if (p && options().includes(p)) return p
    return options()[0] ?? "edits"
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

  // Review comments: a line number opens a box under its line; what's left
  // there goes with the next prompt (the composer says how many).
  const side = (r: LineRow) => (r.kind === "del" ? "old" : "new")
  const lineKey = (r: LineRow) => `${current()?.file}:${side(r)}:${r.line}`
  const [editing, setEditing] = createSignal<string | null>(null)
  const notes = (r: LineRow) => commentsAt(comments[props.sessionID], current()?.file ?? "", r.line, side(r))
  const marked = (r: LineRow) => notes(r).length > 0
  const lineExtras = (r: LineRow) => (
    <>
      <CommentNotes sessionID={props.sessionID} list={notes(r)} />
      <Show when={editing() === lineKey(r)}>
        <CommentBox
          where={`${current()?.file}:${r.line}`}
          onCancel={() => setEditing(null)}
          onSave={(text) => {
            addComment(props.sessionID, { file: current()!.file, line: r.line, side: side(r), code: r.text, text })
            setEditing(null)
          }}
        />
      </Show>
    </>
  )

  /** Revert target: before the picked turn, or before the session's first prompt. */
  const revertTarget = () => {
    const src = source()
    if (src.startsWith("turn:")) return src.slice(5)
    if (src === "edits") return (state.messages[props.sessionID] ?? []).find((m) => state.message[m]?.role === "user")
  }
  // Success shows in place: the turn reads "Rolled back" here, and the
  // transcript dims it above the composer's revert dock.
  const [reverting, setReverting] = createSignal(false)
  const revert = async () => {
    const to = revertTarget()
    if (!to) return toast("Nothing to revert to yet", "error")
    setReverting(true)
    try {
      await revertTo(props.sessionID, to)
      setConfirm(false)
    } catch (err) {
      toast(`Could not revert: ${err instanceof Error ? err.message : err}`, "error")
    } finally {
      setReverting(false)
    }
  }
  const undoRevert = () =>
    unrevert(props.sessionID).catch((err) =>
      toast(`Could not unrevert: ${err instanceof Error ? err.message : err}`, "error"),
    )
  /** The picked turn (or the whole session) is already rolled back. */
  const rolledBack = () => {
    const to = revertTarget()
    return !!to && isReverted(state, props.sessionID, to)
  }
  const revertQuestion = () =>
    (source() === "edits"
      ? `Restore ${files().length} files to before this session?`
      : `Restore files to before this turn? Later turns are undone too.`) +
    (isBusy(state, props.sessionID) ? " The running turn stops first." : "")

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
          <Show when={source() !== "tree" && files().length && !isGit()}>
            <span class="note">Not a git repository: files can't be restored, so there is no revert.</span>
          </Show>
          <Show when={source() !== "tree" && files().length && isGit() && rolledBack()}>
            <span class="mono">Rolled back</span>
            <button class="link-btn" onClick={() => void undoRevert()}>
              Unrevert
            </button>
          </Show>
          <Show when={source() !== "tree" && files().length && isGit() && !rolledBack()}>
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
              <button class="link-btn primary" disabled={reverting()} onClick={() => void revert()}>
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
          <Show when={!current()?.patch && (current()?.additions ?? 0) + (current()?.deletions ?? 0) > 0}>
            <div class="q-empty">
              <p>This turn changed too much to keep every diff, so only this file's line counts are stored.</p>
            </div>
          </Show>
          <Diff rows={rows()} onLine={(r) => setEditing(lineKey(r))} after={lineExtras} marked={marked} />
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
      for (const { file, patch } of toolChanges(tool.tool, st.input ?? {}, st.metadata ?? {})) {
        const name = rel(file)
        if (!name) continue
        const rows = parseUnifiedDiff(patch)
        const add = rows.filter((r) => r.kind === "add").length
        const del = rows.filter((r) => r.kind === "del").length
        const prev = byFile.get(name)
        byFile.set(name, {
          file: name,
          additions: (prev?.additions ?? 0) + add,
          deletions: (prev?.deletions ?? 0) + del,
          patch: prev ? `${prev.patch}\n${patch}` : patch,
        })
      }
    }
  return [...byFile.values()]
}

// The Changes view: what this session changed, file by file, with revert.
// Backed by GET /session/{id}/diff and kept fresh by `session.diff` events.
import { createEffect, createMemo, createSignal, For, Show } from "solid-js"
import { api } from "../../api/client"
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

  const files = createMemo(() => state.diffs[props.sessionID] ?? [])
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

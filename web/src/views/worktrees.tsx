// A git project's worktrees: create one (a new `opencode/<name>` branch in
// its own folder) and start a session in it, start another session in an
// existing one, or remove one. Removing force-deletes the folder and the
// branch, so it asks first, in place. Opened from the project row in the
// table and from the palette.
import { createResource, createSignal, For, Show } from "solid-js"
import { api } from "../api/client"
import type { Project } from "../api/types"
import { projectName } from "../fleet/fleet"
import { state } from "../store/store"
import { basename, tildify } from "../ui/format"
import { DialogHead, dialogId, Modal } from "../ui/modal"
import { server } from "./server"
import { toast } from "./toast"

const message = (err: unknown) => (err instanceof Error ? err.message : String(err))

export function Worktrees(props: {
  project: Project
  onStart: (directory: string) => Promise<void>
  close: () => void
}) {
  const title = dialogId()
  const nameId = dialogId()
  const [list, { refetch }] = createResource(() => api.worktrees(props.project.worktree))
  const [name, setName] = createSignal("")
  const [creating, setCreating] = createSignal(false)
  const [confirm, setConfirm] = createSignal<string | null>(null)
  const sessionsIn = (dir: string) => Object.values(state.sessions).filter((s) => s.directory === dir).length

  const start = async (dir: string) => {
    try {
      await props.onStart(dir)
      props.close()
    } catch (err) {
      toast(`Could not start a session: ${message(err)}`, "error")
    }
  }
  const create = async () => {
    if (creating()) return
    setCreating(true)
    try {
      const info = await api.createWorktree(props.project.worktree, name().trim() || undefined)
      await start(info.directory)
    } catch (err) {
      toast(`Could not create the worktree: ${message(err)}`, "error")
      void refetch()
    } finally {
      setCreating(false)
    }
  }
  const remove = async (dir: string) => {
    setConfirm(null)
    try {
      await api.removeWorktree(props.project.worktree, dir)
    } catch (err) {
      toast(`Could not remove ${basename(dir)}: ${message(err)}`, "error")
    }
    void refetch()
  }

  return (
    <Modal class="manage worktrees" labelledBy={title} onClose={props.close}>
      <DialogHead id={title} title={`Worktrees · ${projectName(props.project)}`} onClose={props.close} />
      <div class="dialog-body">
        <form
          class="connect-field"
          onSubmit={(e) => {
            e.preventDefault()
            void create()
          }}
        >
          <label for={nameId}>New worktree</label>
          <div class="field-row">
            <input
              id={nameId}
              class="other-input"
              autocomplete="off"
              spellcheck={false}
              placeholder="name (optional)"
              value={name()}
              onInput={(e) => setName(e.currentTarget.value)}
            />
            <button class="stamp primary" type="submit" disabled={creating()}>
              {creating() ? "Creating…" : "Create and start"}
            </button>
          </div>
          <p class="connect-note">
            A new branch, <code>opencode/{name().trim() ? slug(name()) : "<name>"}</code>, checked out in its own folder
            under the server's data directory, with a session started in it.
          </p>
        </form>
        <Show when={!list.loading} fallback={<p class="manage-where">Loading…</p>}>
          <Show when={!list.error} fallback={<p class="manage-where">Could not list them: {message(list.error)}</p>}>
            <ul class="archived-list">
              <For each={list()} fallback={<li class="manage-where">No worktrees yet.</li>}>
                {(dir) => (
                  <li>
                    <div class="a-row">
                      <span class="t">{basename(dir)}</span>
                      <span class="m">
                        <bdi>{tildify(dir, server.home)}</bdi>
                        {sessionsIn(dir) ? ` · ${sessionsIn(dir)} sessions` : ""}
                      </span>
                      <button class="link-btn" onClick={() => void start(dir)}>
                        New session
                      </button>
                      <button class="link-btn" aria-expanded={confirm() === dir} onClick={() => setConfirm(dir)}>
                        Remove…
                      </button>
                    </div>
                    <Show when={confirm() === dir}>
                      <div class="confirm-row" role="alert">
                        <span>
                          Remove <b>{basename(dir)}</b>? Its folder and its branch are deleted, with any uncommitted
                          changes and unmerged commits in them.
                        </span>
                        <button class="link-btn danger" onClick={() => void remove(dir)}>
                          Remove
                        </button>
                        <button
                          class="link-btn"
                          ref={(el) => setTimeout(() => el.focus())}
                          onClick={() => setConfirm(null)}
                        >
                          Cancel
                        </button>
                      </div>
                    </Show>
                  </li>
                )}
              </For>
            </ul>
          </Show>
        </Show>
      </div>
    </Modal>
  )
}

/** The server slugifies the name the same way for the branch and folder. */
function slug(name: string) {
  return name
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
}

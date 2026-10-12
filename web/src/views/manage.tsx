// Session management: rename, archive and delete one session, and the list of
// archived sessions. Opened from the table row's ⋯ button, F2 on a table or
// session-list row, and the palette. Archive and delete take the subagent
// sessions under it along; delete asks first, in place.
import { createResource, createSignal, For, Show } from "solid-js"
import { api } from "../api/client"
import type { Session } from "../api/types"
import { displayTitle, placeOf } from "../fleet/fleet"
import { closeSessionColumns } from "../nav/route"
import { state } from "../store/store"
import { age, now, tildify } from "../ui/format"
import { DialogHead, dialogId, Modal } from "../ui/modal"
import { server } from "./server"
import { toast } from "./toast"

/** The session and every subagent session under it, deepest first. */
function family(id: string): string[] {
  const kids = Object.values(state.sessions).filter((s) => s.parentID === id)
  return [...kids.flatMap((k) => family(k.id)), id]
}

const fail = (what: string) => (err: unknown) =>
  toast(`Could not ${what}: ${err instanceof Error ? err.message : String(err)}`, "error")

export async function archiveSession(id: string) {
  const at = Date.now()
  try {
    for (const sid of family(id)) await api.archiveSession(sid, at)
  } catch (err) {
    fail("archive the session")(err)
  }
}

export async function deleteSession(id: string) {
  // Close it first, so the open column doesn't report a deletion as news.
  closeSessionColumns(family(id))
  try {
    await api.deleteSession(id)
    return true
  } catch (err) {
    fail("delete the session")(err)
    return false
  }
}

export function ManageSession(props: { session: Session; close: () => void }) {
  const title = dialogId()
  const nameId = dialogId()
  const [name, setName] = createSignal(props.session.title)
  const [busy, setBusy] = createSignal(false)
  const [confirming, setConfirming] = createSignal(false)
  const subagents = () => family(props.session.id).length - 1
  const renamed = () => name().trim() && name().trim() !== props.session.title

  const rename = async () => {
    if (!renamed() || busy()) return
    setBusy(true)
    try {
      await api.updateSession(props.session.id, { title: name().trim() })
      props.close()
    } catch (err) {
      fail("rename the session")(err)
    } finally {
      setBusy(false)
    }
  }
  // Read the id before closing: the dialog's session prop is gone after.
  const archive = () => {
    const id = props.session.id
    props.close()
    void archiveSession(id)
  }
  const remove = () => {
    const id = props.session.id
    props.close()
    void deleteSession(id)
  }

  return (
    <Modal class="manage" labelledBy={title} onClose={props.close}>
      <DialogHead id={title} title="Session" onClose={props.close} />
      <div class="dialog-body">
        <form
          class="connect-field"
          onSubmit={(e) => {
            e.preventDefault()
            void rename()
          }}
        >
          <label for={nameId}>Title</label>
          <div class="field-row">
            <input
              id={nameId}
              class="other-input"
              autocomplete="off"
              value={name()}
              onInput={(e) => setName(e.currentTarget.value)}
              ref={(el) => setTimeout(() => el.select())}
            />
            <button class="stamp primary" type="submit" disabled={!renamed() || busy()}>
              Rename
            </button>
          </div>
        </form>
        <p class="manage-where">
          {placeOf(props.session).name} · <bdi>{tildify(props.session.directory, server.home)}</bdi>
        </p>
        <div class="manage-acts">
          <div>
            <button class="link-btn" onClick={archive}>
              Archive
            </button>
            <span>
              Leaves the table and the session list{subagents() ? `, with its ${subagents()} subagent sessions` : ""}.
              Archived sessions stay under ⌘K › Archived sessions.
            </span>
          </div>
          <div>
            <button class="link-btn" aria-expanded={confirming()} onClick={() => setConfirming(true)}>
              Delete…
            </button>
            <span>Removes it and its history from the server.</span>
          </div>
          <Show when={confirming()}>
            <ConfirmDelete
              session={props.session}
              subagents={subagents()}
              onDelete={remove}
              onCancel={() => setConfirming(false)}
            />
          </Show>
        </div>
      </div>
    </Modal>
  )
}

function ConfirmDelete(props: { session: Session; subagents: number; onDelete: () => void; onCancel: () => void }) {
  return (
    <div class="confirm-row" role="alert">
      <span>
        Delete <b>{displayTitle(props.session)}</b>
        {props.subagents ? ` and its ${props.subagents} subagent sessions` : ""} for good? This can't be undone.
      </span>
      <button class="link-btn danger" onClick={props.onDelete}>
        Delete
      </button>
      <button class="link-btn" ref={(el) => setTimeout(() => el.focus())} onClick={props.onCancel}>
        Cancel
      </button>
    </div>
  )
}

/** Archived sessions across every project, newest first. */
async function loadArchived(): Promise<Session[]> {
  const out: Session[] = []
  let cursor: number | undefined
  // The list mixes live and archived sessions; walk a few pages for these.
  for (let page = 0; page < 5; page++) {
    const { sessions, next } = await api.sessionsAcrossProjects({ archived: true, roots: true, cursor, limit: 200 })
    out.push(...sessions.filter((s) => s.time.archived))
    if (!next) break
    cursor = next
  }
  return out.sort((a, b) => (b.time.archived ?? 0) - (a.time.archived ?? 0))
}

export function ArchivedSessions(props: { onOpen: (id: string) => void; close: () => void }) {
  const title = dialogId()
  const [list, { mutate }] = createResource(loadArchived)
  const [confirm, setConfirm] = createSignal<string | null>(null)
  const remove = async (s: Session) => {
    setConfirm(null)
    if (await deleteSession(s.id)) mutate((l) => l?.filter((x) => x.id !== s.id))
  }
  return (
    <Modal class="manage archived" labelledBy={title} onClose={props.close}>
      <DialogHead id={title} title="Archived sessions" onClose={props.close} />
      <div class="dialog-body">
        <Show when={!list.loading} fallback={<p class="manage-where">Loading…</p>}>
          <Show when={!list.error} fallback={<p class="manage-where">Could not list them: {String(list.error)}</p>}>
            <ul class="archived-list">
              <For each={list()} fallback={<li class="manage-where">Nothing is archived.</li>}>
                {(s) => (
                  <li>
                    <div class="a-row">
                      <span class="t">{displayTitle(s)}</span>
                      <span class="m">
                        {placeOf(s).name} · archived {age(now() - (s.time.archived ?? 0))} ago
                      </span>
                      <button
                        class="link-btn"
                        onClick={() => {
                          props.close()
                          props.onOpen(s.id)
                        }}
                      >
                        Open
                      </button>
                      <button class="link-btn" aria-expanded={confirm() === s.id} onClick={() => setConfirm(s.id)}>
                        Delete…
                      </button>
                    </div>
                    <Show when={confirm() === s.id}>
                      <ConfirmDelete
                        session={s}
                        subagents={0}
                        onDelete={() => void remove(s)}
                        onCancel={() => setConfirm(null)}
                      />
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

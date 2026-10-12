// The inline pieces of review comments: the box that writes one under a
// line, and the notes already left there.
import { createSignal, For, onMount } from "solid-js"
import { Icon } from "../../ui/icons"
import { removeComment, type LineComment } from "./comments"

export function CommentBox(props: { where: string; onSave: (text: string) => void; onCancel: () => void }) {
  let ta!: HTMLTextAreaElement
  const [text, setText] = createSignal("")
  onMount(() => ta.focus())
  const save = () => text().trim() && props.onSave(text().trim())
  return (
    <div class="cbox">
      <textarea
        ref={ta}
        rows="2"
        aria-label={`Comment on ${props.where}`}
        placeholder={`Comment on ${props.where} — sent with your next prompt`}
        value={text()}
        onInput={(e) => setText(e.currentTarget.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter" && (e.metaKey || e.ctrlKey)) {
            e.preventDefault()
            save()
          } else if (e.key === "Escape") {
            e.preventDefault()
            e.stopPropagation()
            props.onCancel()
          }
        }}
      />
      <div class="cbox-row">
        <span class="hint">⌘↵ add · Esc cancel</span>
        <button class="link-btn" onClick={props.onCancel}>
          Cancel
        </button>
        {/* Ready-Fill: an outlined plate until there is something to add. */}
        <button class="link-btn primary" disabled={!text().trim()} onClick={save}>
          Add comment
        </button>
      </div>
    </div>
  )
}

export function CommentNotes(props: { sessionID: string; list: LineComment[] }) {
  return (
    <For each={props.list}>
      {(c) => (
        <div class="cnote">
          <span class="k">Comment</span>
          <span class="t">{c.text}</span>
          <button class="icon-btn" aria-label="Remove comment" onClick={() => removeComment(props.sessionID, c.id)}>
            <Icon name="close" />
          </button>
        </div>
      )}
    </For>
  )
}

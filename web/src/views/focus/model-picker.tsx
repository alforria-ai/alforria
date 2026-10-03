// The composer's model picker: a filterable listbox over every model the
// server offers, grouped by provider. Typing filters, ↑↓ move, ↵ picks, Esc
// closes back to the button; a press anywhere else closes it.
import { createMemo, createSignal, For, onCleanup, onMount, Show } from "solid-js"
import { openSettings } from "../../nav/route"
import { state } from "../../store/store"
import { Icon } from "../../ui/icons"

export function ModelPicker(props: {
  value?: string
  anchor: () => HTMLElement | undefined
  onPick: (id: string) => void
  onClose: (refocus: boolean) => void
}) {
  let root!: HTMLDivElement
  let input!: HTMLInputElement
  let list!: HTMLDivElement
  const [q, setQ] = createSignal("")
  const all = createMemo(() =>
    Object.entries(state.models)
      .map(([id, info]) => ({ id, name: info.name, provider: id.split("/")[0]! }))
      .sort((a, b) => a.provider.localeCompare(b.provider) || a.name.localeCompare(b.name)),
  )
  const items = createMemo(() => {
    const words = q().toLowerCase().split(/\s+/).filter(Boolean)
    return all().filter((m) => words.every((w) => `${m.name} ${m.id}`.toLowerCase().includes(w)))
  })
  const [sel, setSel] = createSignal(0)
  const sections = createMemo(() => {
    const out: { provider: string; items: { id: string; name: string; i: number }[] }[] = []
    items().forEach((m, i) => {
      const last = out[out.length - 1]
      if (last?.provider === m.provider) last.items.push({ ...m, i })
      else out.push({ provider: m.provider, items: [{ ...m, i }] })
    })
    return out
  })
  const optionId = (i: number) => `mdl-${i}`
  const reveal = () => queueMicrotask(() => list?.querySelector(".on")?.scrollIntoView({ block: "nearest" }))

  onMount(() => {
    setSel(
      Math.max(
        0,
        items().findIndex((m) => m.id === props.value),
      ),
    )
    input.focus()
    reveal()
    const outside = (e: PointerEvent) => {
      const t = e.target as Node
      if (!root.contains(t) && !props.anchor()?.contains(t)) props.onClose(false)
    }
    document.addEventListener("pointerdown", outside, true)
    onCleanup(() => document.removeEventListener("pointerdown", outside, true))
  })

  const pick = (i: number) => {
    const m = items()[i]
    if (m) props.onPick(m.id)
  }
  const onKey = (e: KeyboardEvent) => {
    const n = items().length
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault()
      if (n) setSel((v) => (v + (e.key === "ArrowDown" ? 1 : -1) + n) % n)
      reveal()
    } else if (e.key === "Enter") {
      e.preventDefault()
      pick(sel())
    } else if (e.key === "Escape") {
      e.preventDefault()
      e.stopPropagation()
      props.onClose(true)
    } else if (e.key === "Tab") props.onClose(false)
  }

  return (
    <div class="popover models" ref={root}>
      <input
        ref={input}
        class="pop-filter"
        value={q()}
        placeholder="Filter models"
        aria-label="Filter models"
        autocomplete="off"
        spellcheck={false}
        role="combobox"
        aria-expanded="true"
        aria-controls="mdlList"
        aria-autocomplete="list"
        aria-activedescendant={items().length ? optionId(sel()) : undefined}
        onInput={(e) => {
          setQ(e.currentTarget.value)
          setSel(0)
        }}
        onKeyDown={onKey}
      />
      <div class="pop-list" id="mdlList" role="listbox" aria-label="Models" ref={list}>
        <Show
          when={items().length}
          fallback={
            <div class="empty-pop">
              <Show when={all().length} fallback={<>No models yet. </>}>
                Nothing matches “{q()}”.{" "}
              </Show>
              <button class="link-btn" onClick={() => openSettings("providers")}>
                Connect a provider
              </button>
            </div>
          }
        >
          <For each={sections()}>
            {(sec) => (
              <div role="group" aria-label={sec.provider}>
                <header role="presentation">{sec.provider}</header>
                <For each={sec.items}>
                  {(m) => (
                    <button
                      id={optionId(m.i)}
                      tabindex="-1"
                      role="option"
                      aria-selected={m.i === sel()}
                      classList={{ on: m.i === sel() }}
                      onMouseMove={() => setSel(m.i)}
                      // Keep focus in the filter so the keyboard keeps working.
                      onMouseDown={(e) => e.preventDefault()}
                      onClick={() => pick(m.i)}
                    >
                      {m.name}
                      <span>{m.id.slice(sec.provider.length + 1)}</span>
                      <Show when={m.id === props.value}>
                        <Icon name="check" />
                      </Show>
                    </button>
                  )}
                </For>
              </div>
            )}
          </For>
        </Show>
      </div>
    </div>
  )
}

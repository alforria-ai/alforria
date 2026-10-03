// The identifier band: wordmark, view tabs, server designation, search, and
// the waiting counter, which lights orange only when the human is needed.
import { createEffect, createMemo, on, Show } from "solid-js"
import { interrupts } from "../fleet/fleet"
import { nav, setView } from "../nav/route"
import { now, waited } from "../ui/format"
import { Icon } from "../ui/icons"
import { theme, toggleTheme } from "../ui/theme"
import { server } from "./server"

export function Band(props: {
  onQueue: () => void
  onPalette: () => void
  onNewSession: () => void
  onSettings: () => void
}) {
  const n = () => interrupts().length
  const oldest = createMemo(() => Math.min(...interrupts().map((i) => i.since)))
  let count!: HTMLSpanElement
  // A single tick when the count changes; reduced motion drops it in CSS.
  createEffect(
    on(
      n,
      () => {
        count.classList.remove("tick")
        void count.offsetWidth
        count.classList.add("tick")
      },
      { defer: true },
    ),
  )
  return (
    <header class="band" id="band">
      <a class="wordmark" href="#/" aria-label="alforria, fleet overview">
        alforria
      </a>
      <nav class="band-nav" aria-label="Views">
        <button aria-current={nav.view === "overview"} onClick={() => setView("overview")}>
          <Icon name="rows" />
          Overview<kbd>Esc</kbd>
        </button>
        <button
          aria-current={nav.view === "focus"}
          disabled={!nav.columns.length}
          title={nav.columns.length ? undefined : "Open a session first"}
          onClick={() => setView("focus")}
        >
          <Icon name="columns" />
          Focus
          <Show when={nav.columns.length}>
            <span class="c">{nav.columns.length}</span>
          </Show>
          <kbd>F</kbd>
        </button>
      </nav>
      <button class="designation" title="Server" onClick={props.onSettings}>
        <Icon name="server" />
        <span>SERVER</span>
        <span class="addr">{server.address}</span>
        <Show when={server.version}>
          <span class="dim">{/^\d/.test(server.version) ? `v${server.version}` : server.version}</span>
        </Show>
      </button>
      <button class="cmd" aria-label="Search sessions, files and commands" onClick={props.onPalette}>
        <Icon name="search" />
        <span>Search sessions, files, commands</span>
        <kbd>⌘K</kbd>
      </button>
      <div class="band-actions">
        <button class="band-new" title="New session (N)" onClick={props.onNewSession}>
          <Icon name="plus" />
          <span>New session</span>
          <kbd>N</kbd>
        </button>
        <button
          class="icon-btn theme"
          aria-label={`Switch to ${theme() === "dark" ? "light" : "dark"} theme`}
          onClick={toggleTheme}
        >
          <Icon name="contrast" />
        </button>
        <button class="icon-btn" aria-label="Settings" onClick={props.onSettings}>
          <Icon name="sliders" />
        </button>
      </div>
      <button
        class="wait-count"
        classList={{ lit: n() > 0 }}
        aria-label={`${n()} waiting on you`}
        onClick={props.onQueue}
      >
        <span class="n" ref={count}>
          {n()}
        </span>
        <span class="w">
          <b>Waiting</b>
          <small>{n() ? `oldest ${waited(now() - oldest())}` : "all clear"}</small>
        </span>
      </button>
    </header>
  )
}

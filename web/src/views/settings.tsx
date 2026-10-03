// Settings: providers and credentials, default models, agents, MCP servers,
// permission rules, appearance, and the server itself. Config edits go to the
// global config (every project; a project's opencode.json still overrides).
import { createMemo, createResource, createSignal, For, Match, Show, Switch } from "solid-js"
import { api } from "../api/client"
import { leaveSettings, nav, openSettings } from "../nav/route"
import { state } from "../store/store"
import { Icon } from "../ui/icons"
import { setTheme, theme } from "../ui/theme"
import { server } from "./server"
import { toast } from "./toast"

const PAGES = [
  ["providers", "Providers"],
  ["models", "Models"],
  ["agents", "Agents"],
  ["mcp", "MCP servers"],
  ["permissions", "Permissions"],
  ["appearance", "Appearance"],
  ["server", "Server"],
] as const

/** Any project directory; catalog endpoints are instance-scoped. */
const anyDir = () => state.projects[state.projectOrder[0] ?? ""]?.worktree

const SAVE_NOTE = "Saving reloads every project on this server; a session mid-turn may be interrupted."

export function Settings() {
  return (
    <>
      <div class="pane-head ws-head">
        <button class="back-link" onClick={leaveSettings}>
          <Icon name="back" />
          Back<kbd>Esc</kbd>
        </button>
        <h2>Settings</h2>
      </div>
      <div class="ws-body">
        <div class="settings">
          <nav aria-label="Settings sections">
            <For each={PAGES}>
              {([id, label]) => (
                <button aria-current={nav.settingsPage === id} onClick={() => openSettings(id)}>
                  {label}
                </button>
              )}
            </For>
          </nav>
          <section>
            <Switch fallback={<Providers />}>
              <Match when={nav.settingsPage === "models"}>
                <Models />
              </Match>
              <Match when={nav.settingsPage === "agents"}>
                <Agents />
              </Match>
              <Match when={nav.settingsPage === "mcp"}>
                <Mcp />
              </Match>
              <Match when={nav.settingsPage === "permissions"}>
                <Permissions />
              </Match>
              <Match when={nav.settingsPage === "appearance"}>
                <Appearance />
              </Match>
              <Match when={nav.settingsPage === "server"}>
                <Server />
              </Match>
            </Switch>
          </section>
        </div>
      </div>
    </>
  )
}

function Providers() {
  const [catalog, { refetch }] = createResource(() => api.providerCatalog(anyDir()))
  const [editing, setEditing] = createSignal<string | null>(null)
  const [key, setKey] = createSignal("")
  const rows = createMemo(() => {
    const c = catalog()
    if (!c) return []
    const connected = new Set(c.connected)
    return [...c.all].sort(
      (a, b) => Number(connected.has(b.id)) - Number(connected.has(a.id)) || a.name.localeCompare(b.name),
    )
  })
  const save = async (id: string) => {
    try {
      await api.setAuth(id, key().trim())
      setEditing(null)
      setKey("")
      toast("Credentials saved on the server")
      void refetch()
    } catch (err) {
      toast(`Could not save: ${err instanceof Error ? err.message : err}`, "error")
    }
  }
  const remove = async (id: string) => {
    try {
      await api.removeAuth(id)
      void refetch()
    } catch (err) {
      toast(`Could not remove: ${err instanceof Error ? err.message : err}`, "error")
    }
  }
  return (
    <>
      <h3>Providers</h3>
      <p class="lead">Credentials live on the server in auth.json. The browser never stores keys.</p>
      <Show when={!catalog.loading} fallback={<p class="lead">Loading…</p>}>
        <div class="spec providers">
          <div class="row head">
            <span>Provider</span>
            <span>Id</span>
            <span>Credentials</span>
            <span class="r">Models</span>
            <span />
          </div>
          <For each={rows()}>
            {(p) => {
              const on = () => catalog()!.connected.includes(p.id)
              return (
                <>
                  <div class="row">
                    <span class="n">{p.name}</span>
                    <span class="d mono ep">{p.id}</span>
                    <span class="d cr">
                      {on() ? (p.source === "env" ? `env · ${p.env[0] ?? ""}` : p.source) : "Not configured"}
                    </span>
                    <span class="d mono r md">{Object.keys(p.models).length}</span>
                    <Show
                      when={on()}
                      fallback={
                        <button class="link-btn st" onClick={() => setEditing(p.id)}>
                          Connect
                        </button>
                      }
                    >
                      <span class="st settings-pair">
                        <span class="pill-state on">Connected</span>
                        <Show when={p.source === "api"}>
                          <button class="link-btn" onClick={() => void remove(p.id)}>
                            Remove
                          </button>
                        </Show>
                      </span>
                    </Show>
                  </div>
                  <Show when={editing() === p.id}>
                    <form
                      class="row key-form"
                      onSubmit={(e) => {
                        e.preventDefault()
                        void save(p.id)
                      }}
                    >
                      <input
                        class="other-input"
                        type="password"
                        autocomplete="off"
                        placeholder={`${p.name} API key`}
                        aria-label={`${p.name} API key`}
                        value={key()}
                        onInput={(e) => setKey(e.currentTarget.value)}
                        ref={(el) => setTimeout(() => el.focus())}
                      />
                      <button class="link-btn" type="submit" disabled={!key().trim()}>
                        Save
                      </button>
                      <button class="link-btn" type="button" onClick={() => setEditing(null)}>
                        Cancel
                      </button>
                    </form>
                  </Show>
                </>
              )
            }}
          </For>
        </div>
      </Show>
    </>
  )
}

function useGlobalConfig() {
  const [config, { refetch, mutate }] = createResource(() => api.globalConfig())
  const patch = async (body: Record<string, unknown>) => {
    try {
      mutate(await api.patchGlobalConfig(body))
      toast("Saved")
    } catch (err) {
      toast(`Could not save: ${err instanceof Error ? err.message : err}`, "error")
      void refetch()
    }
  }
  return { config, patch }
}

function Models() {
  const { config, patch } = useGlobalConfig()
  const models = createMemo(() =>
    Object.entries(state.models)
      .map(([id, m]) => ({ id, name: m.name }))
      .sort((a, b) => a.id.localeCompare(b.id)),
  )
  const Select = (p: { field: "model" | "small_model"; label: string; note: string }) => (
    <div class="row">
      <span class="n">{p.label}</span>
      <span class="d">{p.note}</span>
      <select
        class="select"
        aria-label={p.label}
        value={(config()?.[p.field] as string | undefined) ?? ""}
        onChange={(e) => void patch({ [p.field]: e.currentTarget.value || undefined })}
      >
        <option value="">Server default</option>
        <For each={models()}>{(m) => <option value={m.id}>{m.id}</option>}</For>
      </select>
    </div>
  )
  return (
    <>
      <h3>Models</h3>
      <p class="lead">Defaults for new sessions. {SAVE_NOTE}</p>
      <div class="spec">
        <Select field="model" label="Default" note="Used by build and plan unless an agent sets its own." />
        <Select field="small_model" label="Small" note="Titles, summaries and other light work." />
      </div>
    </>
  )
}

function Agents() {
  const [agents] = createResource(() => api.agents(anyDir()))
  return (
    <>
      <h3>Agents</h3>
      <p class="lead">
        Primary agents take prompts; subagents are called by the task tool. Defined in opencode.json and agent files.
      </p>
      <div class="spec">
        <For each={(agents() ?? []).filter((a) => !a.hidden)}>
          {(a) => (
            <div class="row">
              <span class="n">{a.name}</span>
              <span class="d">{a.description ?? ""}</span>
              <span class="pill-state" classList={{ on: a.mode !== "subagent" }}>
                {a.mode === "all" ? "primary + sub" : a.mode}
              </span>
            </div>
          )}
        </For>
      </div>
    </>
  )
}

function Mcp() {
  const [status, { refetch }] = createResource(() => api.mcpStatus(anyDir()))
  const toggle = async (name: string, connected: boolean) => {
    try {
      await (connected ? api.mcpDisconnect(name, anyDir()) : api.mcpConnect(name, anyDir()))
      void refetch()
    } catch (err) {
      toast(`MCP ${name}: ${err instanceof Error ? err.message : err}`, "error")
    }
  }
  return (
    <>
      <h3>MCP servers</h3>
      <p class="lead">Model Context Protocol servers from opencode.json.</p>
      <div class="spec">
        <For
          each={Object.entries(status() ?? {})}
          fallback={
            <div class="row">
              <span class="n">None</span>
              <span class="d">No MCP servers are configured.</span>
              <span />
            </div>
          }
        >
          {([name, st]) => (
            <div class="row">
              <span class="n">{name}</span>
              <span class="d">{st.error ?? st.status}</span>
              <button class="link-btn" onClick={() => void toggle(name, st.status === "connected")}>
                {st.status === "connected" ? "Disconnect" : "Connect"}
              </button>
            </div>
          )}
        </For>
      </div>
    </>
  )
}

type Action = "allow" | "ask" | "deny"
const TOOLS = ["bash", "edit", "read", "webfetch", "websearch", "external_directory", "doom_loop", "task"]

function Permissions() {
  const { config, patch } = useGlobalConfig()
  /** Flatten `permission` config into ordered rules: tool, pattern, action. */
  const rules = createMemo(() => {
    const perm = config()?.permission as string | Record<string, Action | Record<string, Action>> | undefined
    const out: { tool: string; pattern: string; action: Action }[] = []
    if (typeof perm === "string") return [{ tool: "*", pattern: "*", action: perm as Action }]
    for (const tool of new Set([...TOOLS, ...Object.keys(perm ?? {})])) {
      const v = perm?.[tool]
      if (typeof v === "string") out.push({ tool, pattern: "*", action: v })
      else if (v) for (const [pattern, action] of Object.entries(v)) out.push({ tool, pattern, action })
      else out.push({ tool, pattern: "*", action: "default" as Action })
    }
    return out
  })
  const set = (tool: string, pattern: string, action: Action) =>
    void patch({ permission: { [tool]: pattern === "*" ? action : { [pattern]: action } } })
  return (
    <>
      <h3>Permissions</h3>
      <p class="lead">Global rules, evaluated last-match-wins. Anything set to Ask lands in your queue. {SAVE_NOTE}</p>
      <div class="spec numbered">
        <For each={rules()}>
          {(r, i) => (
            <div class="row">
              <span class="no">{String(i() + 1).padStart(2, "0")}</span>
              <span class="n">{r.tool}</span>
              <span class="d mono">{r.pattern}</span>
              <div class="seg perm-seg" role="group" aria-label={`${r.tool} ${r.pattern}`}>
                <For each={["allow", "ask", "deny"] as Action[]}>
                  {(a) => (
                    <button aria-pressed={r.action === a} onClick={() => set(r.tool, r.pattern, a)}>
                      {a}
                    </button>
                  )}
                </For>
              </div>
            </div>
          )}
        </For>
      </div>
    </>
  )
}

function Appearance() {
  return (
    <>
      <h3>Appearance</h3>
      <p class="lead">Stored in this browser only.</p>
      <div class="spec">
        <div class="row">
          <span class="n">Theme</span>
          <span class="d">Dark suits long supervision; light matches the printed datasheet.</span>
          <div class="seg" role="group" aria-label="Theme">
            <For each={["dark", "light"] as const}>
              {(t) => (
                <button aria-pressed={theme() === t} onClick={() => setTheme(t)}>
                  {t}
                </button>
              )}
            </For>
          </div>
        </div>
      </div>
    </>
  )
}

function Server() {
  return (
    <>
      <h3>Server</h3>
      <p class="lead">
        This client talks to one alforria server. A LibertAI account can list more machines here later.
      </p>
      <div class="spec">
        <div class="row">
          <span class="n">Address</span>
          <span class="d mono ok">{server.address}</span>
          <span class="pill-state" classList={{ on: state.conn.state === "live" }}>
            {state.conn.state === "live" ? "Linked" : state.conn.state}
          </span>
        </div>
        <div class="row">
          <span class="n">Version</span>
          <span class="d mono ok">alforria {server.version || "?"} · API v1</span>
          <span />
        </div>
        <div class="row">
          <span class="n">Home</span>
          <span class="d mono ok">{server.home}</span>
          <span />
        </div>
        <div class="row">
          <span class="n">Event stream</span>
          <span class="d mono ok">/global/event · heartbeat 10 s · reconnect 1–30 s</span>
          <span />
        </div>
        <div class="row">
          <span class="n">Projects</span>
          <span class="d mono ok">{state.projectOrder.length}</span>
          <span />
        </div>
      </div>
    </>
  )
}

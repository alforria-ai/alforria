// Workflows: saved agent orchestrations and live run progress. A run fans
// out child sessions; open any agent to read its transcript.
import { createEffect, createMemo, createSignal, For, onCleanup, onMount, Show } from "solid-js"
import { api } from "../api/client"
import type { WorkflowDefinition, WorkflowRun, WorkflowState } from "../api/types"
import { state } from "../store/store"
import { age, clock, now } from "../ui/format"
import { Icon } from "../ui/icons"
import { DialogHead, Modal } from "../ui/modal"
import { toast } from "./toast"

const NEW_SCRIPT = `export const meta = {
  name: 'my-workflow',
  description: 'What this workflow does',
}

phase('Plan')
const plan = await agent('Describe the plan for: ' + args, { label: 'plan' })

phase('Work')
const results = await parallel([
  () => agent('Do the first half. Plan: ' + plan, { label: 'first' }),
  () => agent('Do the second half. Plan: ' + plan, { label: 'second' }),
])

return { plan, results }
`

const TONES: Record<WorkflowState, string> = {
  queued: "queued",
  running: "running",
  completed: "done",
  failed: "fault",
  cancelled: "idle",
}

export function Workflows(props: { onOpen: (session: string) => void }) {
  const projects = createMemo(() =>
    state.projectOrder.flatMap((id) => {
      const project = state.projects[id]
      return project ? [project] : []
    }),
  )
  const [directory, setDirectory] = createSignal(
    (typeof localStorage !== "undefined" && localStorage.getItem("alforria.workflow.directory")) || "",
  )
  const [defs, setDefs] = createSignal<WorkflowDefinition[]>([])
  const [runs, setRuns] = createSignal<WorkflowRun[]>([])
  const [loading, setLoading] = createSignal(false)
  const [selectedRun, setSelectedRun] = createSignal<string | null>(null)
  const [selectedWorkflow, setSelectedWorkflow] = createSignal<string | null>(null)
  const [runTarget, setRunTarget] = createSignal<WorkflowDefinition | null>(null)
  const [editing, setEditing] = createSignal<WorkflowDefinition | "new" | null>(null)

  const current = createMemo(() => runs().find((run) => run.id === selectedRun()))
  const active = () => runs().filter((run) => run.state === "running" || run.state === "queued").length

  createEffect(() => {
    const dir = directory()
    if (!dir && projects().length) setDirectory(projects()[0]!.worktree)
  })

  createEffect(() => {
    const dir = directory()
    if (dir) {
      localStorage.setItem("alforria.workflow.directory", dir)
      void load(dir)
    }
  })

  onMount(() => {
    const timer = setInterval(() => {
      const dir = directory()
      if (dir) void refreshRuns(dir)
    }, 2500)
    onCleanup(() => clearInterval(timer))
  })

  async function load(dir: string) {
    setLoading(true)
    try {
      const [workflows, list] = await Promise.all([api.workflows(dir), api.workflowRuns(dir)])
      setDefs(workflows)
      setRuns(list)
      if (selectedRun() && !list.some((run) => run.id === selectedRun())) setSelectedRun(null)
      if (!selectedRun() && list.length && !selectedWorkflow()) setSelectedRun(list[0]!.id)
    } catch (error) {
      toast(errorText(error), "error")
    } finally {
      setLoading(false)
    }
  }

  async function refreshRuns(dir: string) {
    try {
      setRuns(await api.workflowRuns(dir))
    } catch {
      // A transient poll failure keeps the last snapshot.
    }
  }

  async function start(definition: WorkflowDefinition, args: string) {
    let parsed: unknown = undefined
    if (args.trim()) {
      try {
        parsed = JSON.parse(args)
      } catch {
        toast("Arguments must be valid JSON", "error")
        return false
      }
    }
    try {
      const run = await api.startWorkflow(directory(), { name: definition.name, args: parsed })
      setRunTarget(null)
      setSelectedWorkflow(null)
      setSelectedRun(run.id)
      await load(directory())
      return true
    } catch (error) {
      toast(errorText(error), "error")
      return false
    }
  }

  async function abort(run: WorkflowRun) {
    try {
      const next = await api.abortWorkflow(directory(), run.id)
      setRuns((list) => list.map((item) => (item.id === next.id ? next : item)))
    } catch (error) {
      toast(errorText(error), "error")
    }
  }

  async function remove(definition: WorkflowDefinition) {
    if (!confirm(`Delete workflow "${definition.name}"?`)) return
    try {
      await api.deleteWorkflow(directory(), definition.name)
      if (selectedWorkflow() === definition.name) setSelectedWorkflow(null)
      await load(directory())
    } catch (error) {
      toast(errorText(error), "error")
    }
  }

  return (
    <div class="wf">
      <div class="pane-head ws-head">
        <h2>Workflows</h2>
        <span class="count">
          {defs().length} saved · {active()} running
        </span>
        <label class="wf-dir">
          <Icon name="folder" />
          <select
            aria-label="Project directory"
            value={directory()}
            onChange={(event) => {
              setSelectedRun(null)
              setSelectedWorkflow(null)
              setDirectory(event.currentTarget.value)
            }}
          >
            <Show when={directory() && !projects().some((project) => project.worktree === directory())}>
              <option value={directory()}>{directory()}</option>
            </Show>
            <For each={projects()}>
              {(project) => <option value={project.worktree}>{project.name || project.worktree}</option>}
            </For>
          </select>
        </label>
        <button class="ws-folder" onClick={() => setEditing("new")} disabled={!directory()}>
          <Icon name="plus" />
          <span>New workflow</span>
        </button>
        <button class="icon-btn" aria-label="Refresh" title="Refresh" onClick={() => void load(directory())}>
          <Icon name="undo" />
        </button>
      </div>

      <Show
        when={directory()}
        fallback={
          <div class="q-empty">
            <h3>No project yet</h3>
            <p>Open a session in a folder first; workflows are saved beside the project.</p>
          </div>
        }
      >
        <div class="wf-body">
          <aside class="wf-side" aria-label="Workflows and runs">
            <section class="wf-section">
              <h3>Saved</h3>
              <Show
                when={defs().length}
                fallback={
                  <p class="wf-empty">No saved workflows. Create one, or drop a .js file in .opencode/workflows.</p>
                }
              >
                <For each={defs()}>
                  {(definition) => (
                    <button
                      class="wf-item"
                      aria-current={selectedWorkflow() === definition.name}
                      onClick={() => {
                        setSelectedRun(null)
                        setSelectedWorkflow(definition.name)
                      }}
                    >
                      <span class="wf-name">{definition.name}</span>
                      <span class="wf-meta">
                        <span class={`wf-source ${definition.source}`}>{definition.source}</span>
                        <Show when={definition.description}>
                          <span class="wf-desc">{definition.description}</span>
                        </Show>
                      </span>
                    </button>
                  )}
                </For>
              </Show>
            </section>

            <section class="wf-section">
              <h3>Runs</h3>
              <Show when={runs().length} fallback={<p class="wf-empty">No runs yet.</p>}>
                <For each={runs()}>
                  {(run) => (
                    <button
                      class="wf-item wf-run-item"
                      aria-current={selectedRun() === run.id}
                      onClick={() => {
                        setSelectedWorkflow(null)
                        setSelectedRun(run.id)
                      }}
                    >
                      <span class="wf-name">{run.name}</span>
                      <span class="wf-meta">
                        <span class={`wf-state ${TONES[run.state]}`}>{run.state}</span>
                        <span class="wf-desc">
                          {run.agents.length} agents · {clock(run.started)}
                        </span>
                      </span>
                    </button>
                  )}
                </For>
              </Show>
            </section>
          </aside>

          <section class="wf-main">
            <Show
              when={selectedRun() ? current() : undefined}
              fallback={
                <DefinitionDetail
                  definition={defs().find((item) => item.name === selectedWorkflow())}
                  onRun={(definition) => setRunTarget(definition)}
                  onEdit={(definition) => setEditing(definition)}
                  onDelete={(definition) => void remove(definition)}
                />
              }
            >
              {(run) => <RunDetail run={run()} onOpen={props.onOpen} onAbort={() => void abort(run())} />}
            </Show>
          </section>
        </div>
      </Show>

      <Show when={runTarget()}>
        {(definition) => (
          <RunDialog
            definition={definition()}
            onClose={() => setRunTarget(null)}
            onStart={(args) => start(definition(), args)}
          />
        )}
      </Show>

      <Show when={editing()}>
        {(target) => (
          <EditorDialog
            directory={directory()}
            target={target()}
            onClose={() => setEditing(null)}
            onSaved={async (definition) => {
              setEditing(null)
              setSelectedWorkflow(definition.name)
              setSelectedRun(null)
              await load(directory())
            }}
          />
        )}
      </Show>
    </div>
  )
}

function DefinitionDetail(props: {
  definition?: WorkflowDefinition
  onRun: (definition: WorkflowDefinition) => void
  onEdit: (definition: WorkflowDefinition) => void
  onDelete: (definition: WorkflowDefinition) => void
}) {
  return (
    <Show
      when={props.definition}
      fallback={
        <div class="q-empty">
          <h3>Select a workflow</h3>
          <p>Saved workflows run many agents from one script. Pick one on the left, or create a new one.</p>
        </div>
      }
    >
      {(definition) => (
        <article class="wf-detail">
          <header class="wf-detail-head">
            <div>
              <h3>{definition().name}</h3>
              <p>{definition().description || "No description."}</p>
            </div>
            <div class="wf-actions">
              <button class="link-btn primary" onClick={() => props.onRun(definition())}>
                <Icon name="send" />
                Run
              </button>
              <Show when={definition().source === "project" || definition().source === "global"}>
                <button class="link-btn" onClick={() => props.onEdit(definition())}>
                  Edit
                </button>
                <button class="link-btn" onClick={() => props.onDelete(definition())}>
                  Delete
                </button>
              </Show>
            </div>
          </header>
          <div class="wf-tags">
            <span class={`wf-source ${definition().source}`}>{definition().source}</span>
            <span class="wf-path">{definition().path}</span>
          </div>
          <Show when={definition().phases?.length}>
            <p class="wf-phases">
              Phases: <span>{definition().phases!.join(" → ")}</span>
            </p>
          </Show>
          <pre class="wf-script">{definition().script}</pre>
        </article>
      )}
    </Show>
  )
}

function RunDetail(props: { run: WorkflowRun; onOpen: (session: string) => void; onAbort: () => void }) {
  const phases = createMemo(() => {
    const titles = props.run.phases.map((phase) => phase.title)
    for (const agent of props.run.agents) if (!titles.includes(agent.phase)) titles.push(agent.phase)
    return titles
  })
  const agentsIn = (title: string) => props.run.agents.filter((agent) => agent.phase === title)
  const running = () => props.run.state === "running" || props.run.state === "queued"
  return (
    <article class="wf-detail">
      <header class="wf-detail-head">
        <div>
          <h3>
            {props.run.name}
            <span class={`wf-state ${TONES[props.run.state]}`}>{props.run.state}</span>
          </h3>
          <p>
            {props.run.description || "Workflow run"} · started {clock(props.run.started)}
            {props.run.finished
              ? ` · took ${age(props.run.finished - props.run.started)}`
              : ` · ${age(now() - props.run.started)} elapsed`}
          </p>
        </div>
        <div class="wf-actions">
          <Show when={props.run.sessionID}>
            {(session) => (
              <button class="link-btn" onClick={() => props.onOpen(session())}>
                <Icon name="columns" />
                Open session
              </button>
            )}
          </Show>
          <Show when={running()}>
            <button class="link-btn" onClick={props.onAbort}>
              <Icon name="stop" />
              Stop
            </button>
          </Show>
        </div>
      </header>

      <Show when={props.run.error}>{(error) => <p class="wf-error">{error()}</p>}</Show>

      <Show when={props.run.logs.length}>
        <div class="wf-logs">
          <For each={props.run.logs}>
            {(log) => (
              <p>
                <span>{clock(log.time)}</span>
                {log.message}
              </p>
            )}
          </For>
        </div>
      </Show>

      <div class="wf-phases-list">
        <For each={phases()}>
          {(title) => {
            const phase = () => props.run.phases.find((item) => item.title === title)
            return (
              <section class="wf-phase">
                <header>
                  <h4>{title}</h4>
                  <span class="wf-meta">
                    {agentsIn(title).length} agents
                    <Show when={phase()}>
                      {(item) => <span class={`wf-state ${TONES[item().state]}`}>{item().state}</span>}
                    </Show>
                  </span>
                </header>
                <For each={agentsIn(title)}>
                  {(agent) => (
                    <div class="wf-agent">
                      <div class="wf-agent-head">
                        <span class={`wf-state ${TONES[agent.state]}`}>{agent.state}</span>
                        <span class="wf-agent-label">{agent.label}</span>
                        <span class="wf-agent-kind">@{agent.agent}</span>
                        <span class="wf-agent-time">
                          {agent.finished
                            ? age(agent.finished - agent.started)
                            : running()
                              ? age(now() - agent.started)
                              : ""}
                        </span>
                        <Show when={agent.sessionID}>
                          {(session) => (
                            <button class="wf-open" onClick={() => props.onOpen(session())}>
                              Open
                            </button>
                          )}
                        </Show>
                      </div>
                      <Show when={agent.error}>{(error) => <p class="wf-agent-error">{error()}</p>}</Show>
                      <details class="wf-agent-body">
                        <summary>Prompt{agent.result ? " and result" : ""}</summary>
                        <pre>{agent.prompt}</pre>
                        <Show when={agent.result}>{(result) => <pre class="wf-result">{result()}</pre>}</Show>
                      </details>
                    </div>
                  )}
                </For>
              </section>
            )
          }}
        </For>
      </div>

      <Show when={props.run.result !== undefined && props.run.result !== null}>
        <section class="wf-output">
          <h4>Result</h4>
          <pre>{formatValue(props.run.result)}</pre>
        </section>
      </Show>

      <details class="wf-agent-body wf-script-details">
        <summary>Script</summary>
        <pre>{props.run.script}</pre>
      </details>
    </article>
  )
}

function RunDialog(props: {
  definition: WorkflowDefinition
  onClose: () => void
  onStart: (args: string) => Promise<boolean>
}) {
  const [args, setArgs] = createSignal(props.definition.name === "deep-research" ? '{\n  "question": ""\n}' : "{}")
  const [busy, setBusy] = createSignal(false)
  return (
    <Modal labelledBy="wf-run-title" onClose={props.onClose} class="wf-dialog">
      <DialogHead id="wf-run-title" title={`Run ${props.definition.name}`} onClose={props.onClose} />
      <div class="wf-form">
        <label>
          <span>Arguments (JSON)</span>
          <textarea
            rows={8}
            spellcheck={false}
            value={args()}
            onInput={(event) => setArgs(event.currentTarget.value)}
          />
        </label>
        <p class="wf-hint">
          Passed to the script as <code>args</code>. Use an object for named fields, or a string for a single prompt.
        </p>
      </div>
      <footer class="wf-dialog-actions">
        <button class="link-btn" onClick={props.onClose}>
          Cancel
        </button>
        <button
          class="link-btn primary"
          disabled={busy()}
          onClick={async () => {
            setBusy(true)
            const started = await props.onStart(args())
            setBusy(false)
            if (!started) return
          }}
        >
          <Icon name="send" />
          Start
        </button>
      </footer>
    </Modal>
  )
}

function EditorDialog(props: {
  directory: string
  target: WorkflowDefinition | "new"
  onClose: () => void
  onSaved: (definition: WorkflowDefinition) => void
}) {
  const existing = props.target === "new" ? undefined : props.target
  const [name, setName] = createSignal(existing?.name ?? "my-workflow")
  const [description, setDescription] = createSignal(existing?.description ?? "")
  const [script, setScript] = createSignal(existing?.script ?? NEW_SCRIPT)
  const [scope, setScope] = createSignal<"project" | "global">(existing?.source === "global" ? "global" : "project")
  const [busy, setBusy] = createSignal(false)
  return (
    <Modal labelledBy="wf-edit-title" onClose={props.onClose} class="wf-dialog wf-editor">
      <DialogHead
        id="wf-edit-title"
        title={existing ? `Edit ${existing.name}` : "New workflow"}
        onClose={props.onClose}
      />
      <div class="wf-form">
        <div class="wf-form-row">
          <label>
            <span>Name</span>
            <input value={name()} onInput={(event) => setName(event.currentTarget.value)} disabled={!!existing} />
          </label>
          <label>
            <span>Save to</span>
            <select value={scope()} onChange={(event) => setScope(event.currentTarget.value as "project" | "global")}>
              <option value="project">Project (.opencode/workflows)</option>
              <option value="global">User config</option>
            </select>
          </label>
        </div>
        <label>
          <span>Description</span>
          <input value={description()} onInput={(event) => setDescription(event.currentTarget.value)} />
        </label>
        <label>
          <span>Script</span>
          <textarea
            class="wf-code"
            rows={22}
            spellcheck={false}
            value={script()}
            onInput={(event) => setScript(event.currentTarget.value)}
          />
        </label>
      </div>
      <footer class="wf-dialog-actions">
        <button class="link-btn" onClick={props.onClose}>
          Cancel
        </button>
        <button
          class="link-btn primary"
          disabled={busy() || !name().trim() || !script().trim()}
          onClick={async () => {
            setBusy(true)
            try {
              const saved = await api.saveWorkflow(props.directory, {
                name: name().trim(),
                description: description().trim() || undefined,
                script: script(),
                scope: scope(),
              })
              props.onSaved(saved)
            } catch (error) {
              toast(errorText(error), "error")
            } finally {
              setBusy(false)
            }
          }}
        >
          Save
        </button>
      </footer>
    </Modal>
  )
}

function formatValue(value: unknown): string {
  if (typeof value === "string") return value
  return JSON.stringify(value, null, 2)
}

function errorText(error: unknown): string {
  return error instanceof Error ? error.message : String(error)
}

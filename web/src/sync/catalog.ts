// Per-directory catalogs. Each project directory has its own instance and
// config, so what it offers differs: agents and slash commands (cached here
// for the composer), and models, MCP and LSP status (kept in the store under
// `state.catalogs[directory]`). An instance re-reads its config when it is
// disposed, so a disposal drops the cached lists, bumps `catalogEpoch` for
// resources keyed on it, and re-reads the stored catalogs a view uses.
import { createEffect, createSignal, on } from "solid-js"
import { api, type ProviderList } from "../api/client"
import type { Agent, Command } from "../api/types"
import { setState, state } from "../store/store"
import type { Catalog, ModelInfo } from "../store/state"

const agents = new Map<string, Promise<Agent[]>>()
const commands = new Map<string, Promise<Command[]>>()
const [epoch, setEpoch] = createSignal(0)

/** Changes whenever cached catalogs are dropped. */
export const catalogEpoch = epoch

function cached<T>(cache: Map<string, Promise<T[]>>, dir: string, load: () => Promise<T[]>) {
  let p = cache.get(dir)
  if (!p) {
    p = load().catch(() => {
      // Don't keep a failure: the next read tries again.
      cache.delete(dir)
      return []
    })
    cache.set(dir, p)
  }
  return p
}

export const agentsFor = (dir: string) => cached(agents, dir, () => api.agents(dir))
export const commandsFor = (dir: string) => cached(commands, dir, () => api.commands(dir))

/** `providerID/modelID` → display info and reasoning variants, from GET /config/providers. */
export function modelMap(list: ProviderList) {
  const models: Record<string, ModelInfo> = {}
  for (const p of list.providers)
    for (const m of Object.values(p.models)) {
      const info: ModelInfo = { name: m.name, context: m.limit?.context }
      const variants = Object.entries(m.variants ?? {})
        .filter(([, v]) => !v?.disabled)
        .map(([name]) => name)
      if (variants.length) info.variants = variants
      models[`${p.id}/${m.id}`] = info
    }
  return models
}

const readers: { [K in keyof Catalog]-?: (dir: string) => Promise<Catalog[K]> } = {
  models: async (dir) => modelMap(await api.providers(dir)),
  mcp: (dir) => api.mcpStatus(dir),
  lsp: (dir) => api.lspStatus(dir),
}

/**
 * Read a directory's catalog into the store: every part, or only `parts`.
 * A part that fails to load keeps what the store had.
 */
export async function loadCatalog(dir: string, parts = Object.keys(readers) as (keyof Catalog)[]) {
  await Promise.all(
    parts.map(async (part) => {
      const value = await readers[part](dir).catch(() => undefined)
      if (value !== undefined) setState("catalogs", dir, (c) => ({ ...c, [part]: value }))
    }),
  )
}

/** Keep `dir()`'s catalog loaded while the calling view is mounted (read once per directory). */
export function useCatalog(dir: () => string | undefined) {
  createEffect(
    on(dir, (d) => {
      if (d && !state.catalogs[d]) void loadCatalog(d)
    }),
  )
}

/** The models a session in `dir` can use: its directory's list once read, else the server-wide one. */
export function modelsFor(dir: string | undefined): Record<string, ModelInfo> {
  return (dir && state.catalogs[dir]?.models) || state.models
}

/** Forget one directory's catalogs, or every directory's; stored ones are read again. */
export function dropCatalog(dir?: string) {
  if (dir === undefined) {
    agents.clear()
    commands.clear()
  } else {
    agents.delete(dir)
    commands.delete(dir)
  }
  setEpoch((n) => n + 1)
  for (const d of Object.keys(state.catalogs)) if (dir === undefined || d === dir) void loadCatalog(d)
}

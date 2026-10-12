// Per-directory catalogs the composer offers: agents and slash commands. They
// come from the directory's config, which its instance re-reads when it is
// disposed, so a disposal drops the cached lists and bumps `catalogEpoch` for
// resources keyed on it to refetch.
import { createSignal } from "solid-js"
import { api } from "../api/client"
import type { Agent, Command } from "../api/types"

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

/** Forget one directory's catalogs, or every directory's. */
export function dropCatalog(dir?: string) {
  if (dir === undefined) {
    agents.clear()
    commands.clear()
  } else {
    agents.delete(dir)
    commands.delete(dir)
  }
  setEpoch((n) => n + 1)
}

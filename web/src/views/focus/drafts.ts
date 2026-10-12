// Composer drafts survive a reload: each session's text, agent, model and
// variant are kept in localStorage (attachments are not: data URLs are large
// and the file is a pick away). Storage can be missing, full or blocked, so
// every access is guarded and the composer works without it.

export interface StoredDraft {
  text: string
  agent?: string
  model?: string
  variant?: string
  files: string[]
}

const KEY = "alforria.drafts"
/** Keep the most recently started drafts; older ones go first. */
const MAX = 100

type Storage = Pick<globalThis.Storage, "getItem" | "setItem" | "removeItem">

const local = (): Storage | undefined => {
  try {
    return globalThis.localStorage
  } catch {
    return undefined
  }
}

export function loadDrafts(store: Storage | undefined = local()): Record<string, StoredDraft> {
  try {
    const raw = store?.getItem(KEY)
    const parsed: unknown = raw ? JSON.parse(raw) : {}
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return {}
    const out: Record<string, StoredDraft> = {}
    for (const [id, d] of Object.entries(parsed as Record<string, Partial<StoredDraft>>))
      if (d && typeof d.text === "string")
        out[id] = {
          text: d.text,
          agent: typeof d.agent === "string" ? d.agent : undefined,
          model: typeof d.model === "string" ? d.model : undefined,
          variant: typeof d.variant === "string" ? d.variant : undefined,
          files: Array.isArray(d.files) ? d.files.filter((f) => typeof f === "string") : [],
        }
    return out
  } catch {
    return {}
  }
}

/** Only what's worth restoring: drafts with text or a choice, at most MAX. */
export function saveDrafts(drafts: Record<string, StoredDraft>, store: Storage | undefined = local()) {
  const keep = Object.entries(drafts).filter(
    ([, d]) => d.text.trim() || d.agent !== undefined || d.model !== undefined || d.variant !== undefined,
  )
  const out = Object.fromEntries(
    keep
      .slice(-MAX)
      .map(([id, d]) => [id, { text: d.text, agent: d.agent, model: d.model, variant: d.variant, files: d.files }]),
  )
  try {
    if (Object.keys(out).length) store?.setItem(KEY, JSON.stringify(out))
    else store?.removeItem(KEY)
  } catch {
    /* full or blocked: the draft lives on in memory */
  }
}

/**
 * ↑/↓ through earlier prompts. `index` is the position in `history` being
 * shown (undefined: the draft itself). Returns the next index, or undefined
 * back at the draft; `stay` when there is nowhere to go.
 */
export function historyStep(
  history: string[],
  index: number | undefined,
  dir: "up" | "down",
): { index: number | undefined } | "stay" {
  if (dir === "up") {
    if (!history.length || index === 0) return "stay"
    return { index: index === undefined ? history.length - 1 : index - 1 }
  }
  if (index === undefined) return "stay"
  return { index: index + 1 < history.length ? index + 1 : undefined }
}

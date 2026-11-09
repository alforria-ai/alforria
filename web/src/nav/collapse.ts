// Which parents show their subagents in the session lists (the overview
// table and the focus rail, which share it). One default for every parent,
// plus the parents toggled away from it. A per-viewer convenience, kept in
// this browser; without storage it just starts over on reload.
import { createStore, produce } from "solid-js/store"

const KEY = "alforria.subagents"
/** Parents remembered as toggled away from the default; the oldest go first. */
const MAX_EXCEPT = 500

interface Folds {
  /** The default: every parent's subagents hidden. */
  collapsed: boolean
  /** Parents toggled away from the default, in toggle order. */
  except: Record<string, true>
}

function stored(): Folds {
  try {
    const v = JSON.parse(localStorage.getItem(KEY) ?? "null")
    if (v && typeof v.collapsed === "boolean" && v.except && typeof v.except === "object")
      return { collapsed: v.collapsed, except: v.except }
  } catch {
    /* storage unavailable or unreadable: start from the default */
  }
  return { collapsed: false, except: {} }
}

const [folds, setFolds] = createStore<Folds>(
  typeof window === "undefined" ? { collapsed: false, except: {} } : stored(),
)

function save() {
  try {
    localStorage.setItem(KEY, JSON.stringify(folds))
  } catch {
    /* it just won't survive a reload */
  }
}

/** Whether this parent's subagents are hidden. */
export const subagentsCollapsed = (parentID: string) => folds.collapsed !== !!folds.except[parentID]

/** Hide or show one parent's subagents. */
export function setSubagentsCollapsed(parentID: string, collapsed: boolean) {
  if (subagentsCollapsed(parentID) === collapsed) return
  setFolds(
    produce((f) => {
      if (f.except[parentID]) delete f.except[parentID]
      else {
        f.except[parentID] = true
        const keys = Object.keys(f.except)
        for (const k of keys.slice(0, Math.max(0, keys.length - MAX_EXCEPT))) delete f.except[k]
      }
    }),
  )
  save()
}

export const toggleSubagents = (parentID: string) => setSubagentsCollapsed(parentID, !subagentsCollapsed(parentID))

/** Every parent, now and from now on (new parents follow the default). */
export function setAllSubagentsCollapsed(collapsed: boolean) {
  setFolds({ collapsed, except: {} })
  save()
}

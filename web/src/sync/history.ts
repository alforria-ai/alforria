// Sessions beyond the fleet's history window: every project's, newest first,
// read a page at a time from GET /experimental/session. Asking it boots no
// instance, so it is safe to walk however far back the caller needs.
import { api, type GlobalSession } from "../api/client"

/** Pages read at most per walk (200 sessions each). */
const MAX_PAGES = 50

/**
 * Walk the server's sessions page by page; `onPage` returns false to stop
 * early. `start` keeps sessions updated since then, `search` matches titles.
 */
export async function eachSessionPage(
  query: { start?: number; search?: string },
  onPage: (items: GlobalSession[]) => boolean | void,
) {
  let cursor: number | undefined
  for (let page = 0; page < MAX_PAGES; page++) {
    const { items, next } = await api.allSessions({ ...query, cursor, limit: 200 })
    if (onPage(items) === false || next === undefined) return
    cursor = next
  }
}

/** Sessions whose title matches `search`, however old: the "All sessions" search. */
export async function searchSessions(search: string, limit = 50) {
  const out: GlobalSession[] = []
  await eachSessionPage({ search }, (items) => {
    out.push(...items)
    return out.length < limit
  })
  return out.slice(0, limit)
}

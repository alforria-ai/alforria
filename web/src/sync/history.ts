// Sessions beyond the fleet's history window: every project's, newest first,
// read a page at a time from GET /experimental/session. Asking it boots no
// instance, so it is safe to walk however far back the caller needs.
import { api, type GlobalSession } from "../api/client"

/** Pages read at most per walk, and sessions per page. */
const MAX_PAGES = 50
const PAGE_SIZE = 200
/** The most sessions one walk reads. */
export const MAX_SESSIONS = MAX_PAGES * PAGE_SIZE

/**
 * Walk the server's sessions page by page; `onPage` returns false to stop
 * early. `start` keeps sessions updated since then, `search` matches titles,
 * `roots` leaves subagent sessions out. Resolves true once the last page is
 * read, false when stopped early or capped at `MAX_PAGES`.
 */
export async function eachSessionPage(
  query: { start?: number; search?: string; roots?: boolean },
  onPage: (items: GlobalSession[]) => boolean | void,
) {
  let cursor: number | undefined
  for (let page = 0; page < MAX_PAGES; page++) {
    const { items, next } = await api.allSessions({ ...query, cursor, limit: PAGE_SIZE })
    if (onPage(items) === false) return false
    if (next === undefined) return true
    cursor = next
  }
  return false
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

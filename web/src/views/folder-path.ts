// Path arithmetic for the folder picker. Paths are POSIX: the server is
// what runs the sessions, and the input is typed against its file system.

/** Collapse `.`, `..` and repeated slashes; never climbs above "/". */
export function canonical(path: string) {
  const out: string[] = []
  for (const part of path.split("/")) {
    if (!part || part === ".") continue
    if (part === "..") out.pop()
    else out.push(part)
  }
  return `/${out.join("/")}`
}

/** An absolute path for what was typed: `~` is the server's home, a bare name is under it. */
export function expand(input: string, home: string) {
  const root = home || "/"
  if (input === "~" || input.startsWith("~/")) return canonical(`${root}/${input.slice(1)}`)
  if (input.startsWith("/")) return canonical(input)
  return canonical(`${root}/${input}`)
}

/**
 * Split the input at its last slash: the folder being listed, and the name
 * being typed inside it. "~/repos/al" lists ~/repos for names matching "al".
 */
export function splitInput(input: string, home: string) {
  const cut = input.lastIndexOf("/")
  if (cut < 0) return input === "~" ? { dir: expand("~", home), tail: "" } : { dir: expand("~", home), tail: input }
  return { dir: expand(input.slice(0, cut + 1), home), tail: input.slice(cut + 1) }
}

/** How a folder is written back into the input: under home as `~/…`, always with a trailing slash. */
export function inputFor(path: string, home: string) {
  const short =
    home && home !== "/" && (path === home || path.startsWith(`${home}/`)) ? `~${path.slice(home.length)}` : path
  return short.endsWith("/") ? short : `${short}/`
}

export function parentOf(path: string) {
  return canonical(`${path}/..`)
}

export function joinPath(dir: string, name: string) {
  return dir === "/" ? `/${name}` : `${dir}/${name}`
}

/**
 * Subfolder names matching what was typed: names starting with it first,
 * then names containing it, each alphabetical. Dot folders show only once
 * the typed name starts with a dot.
 */
export function matchFolders(names: readonly string[], tail: string) {
  const q = tail.toLowerCase()
  const shown = names.filter((n) => q.startsWith(".") || !n.startsWith("."))
  const byName = (a: string, b: string) => a.localeCompare(b, undefined, { sensitivity: "base", numeric: true })
  if (!q) return [...shown].sort(byName)
  const starts = shown.filter((n) => n.toLowerCase().startsWith(q)).sort(byName)
  const contains = shown.filter((n) => !n.toLowerCase().startsWith(q) && n.toLowerCase().includes(q)).sort(byName)
  return [...starts, ...contains]
}

/**
 * Where a listing of `path` is anchored: the server's home for anything under
 * it, else the root. The server lists only inside its base, and one base
 * means one instance for every folder browsed.
 */
export function listingBase(path: string, home: string) {
  return home && home !== "/" && (path === home || path.startsWith(`${home}/`)) ? home : "/"
}

/** Subfolder names from a `GET /api/fs/list` answer (paths relative to the base, folders ending in "/"). */
export function folderNames(entries: readonly { path: string; type: string }[]) {
  return entries
    .filter((e) => e.type === "directory")
    .map((e) => e.path.replace(/\/+$/, "").split("/").pop() ?? "")
    .filter(Boolean)
}

/**
 * Every folder a session ran in, plus every project's worktree, most recently
 * used first, once each. "/" (the non-git project's worktree) is no place.
 */
export function recentFolders(
  sessions: readonly { directory: string; time: { updated: number } }[],
  projects: readonly { worktree: string; time: { updated?: number; created: number } }[],
) {
  const last = new Map<string, number>()
  const see = (dir: string, at: number) => {
    if (dir && dir !== "/" && at > (last.get(dir) ?? -1)) last.set(dir, at)
  }
  for (const s of sessions) see(s.directory, s.time.updated)
  for (const p of projects) see(p.worktree, p.time.updated ?? p.time.created)
  return [...last.entries()].sort((a, b) => b[1] - a[1]).map(([dir]) => dir)
}

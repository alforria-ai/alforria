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

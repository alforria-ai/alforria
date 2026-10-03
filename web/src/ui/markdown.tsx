// Assistant prose. GFM is parsed by marked and rendered to HTML strings that
// only ever contain tags this file emits: raw HTML in the source shows as text,
// and links open only for http(s) and mailto. Code blocks are highlighted by
// shiki, loaded on first need, with a monochrome theme mapped onto classes
// (DESIGN.md: Diffs and Code). While text streams, only its unsettled tail is
// re-lexed and re-rendered, so settled blocks keep their DOM and highlighting.
import { Marked, type RendererObject, type Token, type Tokens } from "marked"
import { createEffect, onCleanup, type JSX } from "solid-js"
import type { HighlighterCore, LanguageRegistration, ThemedToken, ThemeRegistration } from "shiki/core"
import "./markdown.css"

const ENTITY: Record<string, string> = { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }
/** Escape everything: code and attribute values. */
const esc = (s: string) => s.replace(/[&<>"']/g, (c) => ENTITY[c]!)
/** Escape prose, keeping character references (`&mdash;`, `&#8212;`) as markdown means them. */
const escText = (s: string) => s.replace(/[<>"']|&(?!#\d{1,7};|#[xX][\da-fA-F]{1,6};|\w+;)/g, (c) => ENTITY[c]!)

/** The only URLs that become links. Checked on the raw string, which is then fully escaped. */
const safeHref = (href: string) => (/^(?:https?:|mailto:)/i.test(href) ? href : undefined)
const anchor = (href: string, inner: string, title?: string | null) =>
  `<a href="${esc(href)}"${title ? ` title="${esc(title)}"` : ""} target="_blank" rel="noreferrer noopener">${inner}</a>`

/** Short numeric cells (counts, sizes, durations, money) are data and set in mono. */
const NUMERIC = /^[~≈±+\-−]?[$€£¥]?\d[\d\s,.:/'_+\-−×x]*(?:%|[a-zA-Zµ]{1,4})?$/

// ---- highlighting ----------------------------------------------------------

type Grammar = () => Promise<{ default: LanguageRegistration[] }>
const GRAMMARS: Record<string, Grammar> = {
  c: () => import("shiki/langs/c.mjs"),
  cpp: () => import("shiki/langs/cpp.mjs"),
  csharp: () => import("shiki/langs/csharp.mjs"),
  css: () => import("shiki/langs/css.mjs"),
  diff: () => import("shiki/langs/diff.mjs"),
  docker: () => import("shiki/langs/docker.mjs"),
  dotenv: () => import("shiki/langs/dotenv.mjs"),
  elixir: () => import("shiki/langs/elixir.mjs"),
  go: () => import("shiki/langs/go.mjs"),
  graphql: () => import("shiki/langs/graphql.mjs"),
  haskell: () => import("shiki/langs/haskell.mjs"),
  hcl: () => import("shiki/langs/hcl.mjs"),
  html: () => import("shiki/langs/html.mjs"),
  ini: () => import("shiki/langs/ini.mjs"),
  java: () => import("shiki/langs/java.mjs"),
  javascript: () => import("shiki/langs/javascript.mjs"),
  json: () => import("shiki/langs/json.mjs"),
  jsonc: () => import("shiki/langs/jsonc.mjs"),
  jsx: () => import("shiki/langs/jsx.mjs"),
  kotlin: () => import("shiki/langs/kotlin.mjs"),
  lua: () => import("shiki/langs/lua.mjs"),
  make: () => import("shiki/langs/make.mjs"),
  markdown: () => import("shiki/langs/markdown.mjs"),
  nix: () => import("shiki/langs/nix.mjs"),
  php: () => import("shiki/langs/php.mjs"),
  powershell: () => import("shiki/langs/powershell.mjs"),
  proto: () => import("shiki/langs/proto.mjs"),
  python: () => import("shiki/langs/python.mjs"),
  ruby: () => import("shiki/langs/ruby.mjs"),
  rust: () => import("shiki/langs/rust.mjs"),
  scss: () => import("shiki/langs/scss.mjs"),
  shellscript: () => import("shiki/langs/shellscript.mjs"),
  shellsession: () => import("shiki/langs/shellsession.mjs"),
  sql: () => import("shiki/langs/sql.mjs"),
  swift: () => import("shiki/langs/swift.mjs"),
  terraform: () => import("shiki/langs/terraform.mjs"),
  toml: () => import("shiki/langs/toml.mjs"),
  tsx: () => import("shiki/langs/tsx.mjs"),
  typescript: () => import("shiki/langs/typescript.mjs"),
  xml: () => import("shiki/langs/xml.mjs"),
  yaml: () => import("shiki/langs/yaml.mjs"),
  zig: () => import("shiki/langs/zig.mjs"),
}
const ALIASES: Record<string, string> = {
  bash: "shellscript",
  sh: "shellscript",
  shell: "shellscript",
  zsh: "shellscript",
  console: "shellsession",
  "c++": "cpp",
  cc: "cpp",
  cxx: "cpp",
  hpp: "cpp",
  h: "c",
  cs: "csharp",
  "c#": "csharp",
  dockerfile: "docker",
  containerfile: "docker",
  env: "dotenv",
  ex: "elixir",
  exs: "elixir",
  golang: "go",
  gql: "graphql",
  hs: "haskell",
  htm: "html",
  js: "javascript",
  mjs: "javascript",
  cjs: "javascript",
  json5: "jsonc",
  kt: "kotlin",
  kts: "kotlin",
  makefile: "make",
  md: "markdown",
  patch: "diff",
  ps1: "powershell",
  pwsh: "powershell",
  protobuf: "proto",
  py: "python",
  rb: "ruby",
  rs: "rust",
  tf: "terraform",
  ts: "typescript",
  mts: "typescript",
  cts: "typescript",
  svg: "xml",
  yml: "yaml",
}
const grammarOf = (label: string) => {
  const id = ALIASES[label] ?? label
  return id in GRAMMARS ? id : undefined
}

// The theme paints token kinds with sentinel colours that map back to classes,
// so markdown.css owns the actual colours and the light theme follows for free.
const FG = "#010101"
const CLASS: Record<string, string> = { "#020202": "kw", "#030303": "st", "#040404": "cm" }
const THEME: ThemeRegistration = {
  name: "alforria-mono",
  type: "dark",
  fg: FG,
  bg: "#000000",
  settings: [
    { settings: { foreground: FG, background: "#000000" } },
    { scope: ["comment", "punctuation.definition.comment"], settings: { foreground: "#040404" } },
    {
      scope: ["string", "punctuation.definition.string", "constant.character.escape"],
      settings: { foreground: "#030303" },
    },
    {
      scope: ["keyword", "storage", "keyword.operator.new", "keyword.operator.expression", "keyword.operator.word"],
      settings: { foreground: "#020202" },
    },
    { scope: ["keyword.operator", "string.regexp"], settings: { foreground: FG } },
  ],
}

/** Blocks larger than this stay plain: tokenizing them would stall the transcript. */
const MAX_CODE = 20_000

interface Job {
  lang: string
  code: string
}
const keyOf = (j: Job) => `${j.lang}\0${j.code}`

let core: Promise<HighlighterCore> | undefined
const grammars = new Map<string, Promise<boolean>>()
/** Highlighted HTML by language and code, shared by every transcript. Failures land here as plain text. */
const highlighted = new Map<string, string>()

function remember(key: string, html: string) {
  highlighted.set(key, html)
  if (highlighted.size > 500) highlighted.delete(highlighted.keys().next().value!)
}

/** True once anything has asked for shiki. Text without code never does. */
export const highlighterRequested = () => core !== undefined

/** Highlight `jobs` into the shared cache, loading shiki and the grammars they need. */
export async function highlight(jobs: Job[]): Promise<void> {
  if (!jobs.length) return
  core ??= Promise.all([import("shiki/core"), import("shiki/engine/javascript")]).then(
    ([{ createHighlighterCore }, { createJavaScriptRegexEngine }]) =>
      createHighlighterCore({ themes: [THEME], langs: [], engine: createJavaScriptRegexEngine({ forgiving: true }) }),
  )
  const hl = await core.catch(() => undefined)
  const ready = new Map<string, boolean>()
  for (const lang of new Set(jobs.map((j) => j.lang))) {
    let p = grammars.get(lang)
    if (!p && hl) {
      p = GRAMMARS[lang]!().then(
        (m) => hl.loadLanguage(m.default).then(() => true),
        () => false,
      )
      grammars.set(lang, p)
    }
    ready.set(lang, (await p) ?? false)
  }
  for (const j of jobs) {
    const key = keyOf(j)
    if (highlighted.has(key)) continue
    let html = esc(j.code)
    if (hl && ready.get(j.lang)) {
      try {
        html = tokensHtml(
          hl.codeToTokensBase(j.code, { lang: j.lang, theme: THEME.name!, tokenizeMaxLineLength: 1000 }),
        )
      } catch {}
    }
    remember(key, html)
  }
}

function tokensHtml(lines: ThemedToken[][]): string {
  const span = (cls: string, text: string) => (cls ? `<span class="${cls}">${esc(text)}</span>` : esc(text))
  return lines
    .map((line) => {
      let out = ""
      let cls = ""
      let run = ""
      for (const t of line) {
        const c = CLASS[t.color?.toLowerCase() ?? ""] ?? ""
        // Whitespace joins whatever run it sits in, which keeps spans few.
        if (c !== cls && t.content.trim()) {
          if (run) out += span(cls, run)
          cls = c
          run = ""
        }
        run += t.content
      }
      return run ? out + span(cls, run) : out
    })
    .join("\n")
}

// ---- rendering -------------------------------------------------------------

/** Highlight work found by the render in progress. */
let found: Job[] = []

const renderer: RendererObject = {
  html({ text, block }) {
    const t = text.trim()
    if (/^<!--[\s\S]*-->$/.test(t)) return ""
    if (/^<br\s*\/?>$/i.test(t)) return "<br>"
    return block ? `<p>${escText(t)}</p>\n` : escText(text)
  },
  text(token) {
    return "tokens" in token && token.tokens ? this.parser.parseInline(token.tokens) : escText(token.text)
  },
  codespan({ text }) {
    return `<code>${esc(text)}</code>`
  },
  link({ href, title, tokens }) {
    const inner = this.parser.parseInline(tokens)
    const url = safeHref(href.trim())
    return url ? anchor(url, inner, title) : inner
  },
  // Images never load: a remote image is a tracking pixel. It becomes a link instead.
  image({ href, text }) {
    const url = safeHref(href.trim())
    const label = escText(text || href)
    return url ? anchor(url, label) : label
  },
  code({ text, lang }) {
    const label = (lang ?? "")
      .trim()
      .split(/\s/)[0]!
      .toLowerCase()
      .replace(/[^\w.+#-]/g, "")
      .slice(0, 24)
    const code = text.replace(/\n$/, "")
    const grammar = grammarOf(label)
    const job = grammar && code.length <= MAX_CODE ? { lang: grammar, code } : undefined
    const done = job && highlighted.get(keyOf(job))
    if (job && done === undefined) found.push(job)
    return (
      `<pre class="code-block"><div class="code-head"><span class="lang">${esc(label || "text")}</span>` +
      `<button type="button" class="copy">Copy</button></div><code>${done ?? esc(code)}</code></pre>\n`
    )
  },
  table(token) {
    const row = (cells: Tokens.TableCell[]) => `<tr>${cells.map((c) => this.tablecell(c)).join("")}</tr>\n`
    const body = token.rows.length ? `<tbody>\n${token.rows.map(row).join("")}</tbody>` : ""
    return `<div class="table"><table>\n<thead>\n${row(token.header)}</thead>\n${body}</table></div>\n`
  },
  tablecell({ tokens, text, header, align }) {
    const tag = header ? "th" : "td"
    const num = !header && NUMERIC.test(text.replace(/[*_~`]/g, "").trim())
    return `<${tag}${align ? ` align="${align}"` : ""}${num ? ` class="num"` : ""}>${this.parser.parseInline(tokens)}</${tag}>`
  },
  listitem(item) {
    return `<li${item.task ? ` class="task"` : ""}>${this.parser.parse(item.tokens)}</li>\n`
  },
}

const md = new Marked({ gfm: true, breaks: false, renderer })
const normalize = (text: string) => text.replace(/\r\n?/g, "\n")

/** Render a whole document. Pure: it uses highlighting already cached, and never loads any. */
export function renderMarkdown(text: string): string {
  found = []
  try {
    return md.parser(md.lexer(normalize(text)))
  } catch {
    return `<p>${esc(text)}</p>`
  }
}

interface Block {
  token: Token
  html: string
  /** Code in this block that was rendered plain and still wants highlighting. */
  jobs: Job[]
}

function renderBlock(token: Token): Block {
  found = []
  let html: string
  try {
    html = md.parser([token])
  } catch {
    html = `<p>${esc(token.raw)}</p>`
  }
  return { token, html, jobs: found }
}

const waiting = (b: Block) => b.jobs.some((j) => highlighted.has(keyOf(j)))
const isSpace = (b: Block) => b.token.type === "space"

/**
 * Whether the tokens' raw text tiles `src` exactly. marked garbles `raw` in a
 * few cases (a quoted list with a lazy line, a repeated reference definition),
 * and a tail lexed from a wrong offset would render the wrong text.
 */
function covers(tokens: Token[], src: string) {
  let at = 0
  for (const t of tokens) {
    if (!src.startsWith(t.raw, at)) return false
    at += t.raw.length
  }
  return at === src.length
}

/** Index of the `n`th non-blank block from the end, or 0. */
function fromEnd(blocks: Block[], n: number) {
  for (let i = blocks.length - 1; i >= 0; i--) if (!isSpace(blocks[i]!) && --n === 0) return i
  return 0
}

/**
 * A document that is re-rendered as it grows. When new text only appends, all
 * but the last two blocks are settled (a paragraph can still become a setext
 * heading or a table header once the next line arrives), so only the tail is
 * re-lexed. Blocks whose output did not change keep their identity, which lets
 * the DOM keep them.
 */
export class MarkdownStream {
  private text = ""
  private blocks: Block[] = []
  /** Reference definitions make any block depend on the whole text: then always lex all of it. */
  private links = ""
  /** Whether block offsets can be trusted (see `covers`); if not, always lex all of it. */
  private exact = true

  update(next: string): Block[] {
    next = normalize(next)
    if (next === this.text) return this.blocks
    const old = this.blocks
    let keep = !this.links && this.exact && next.startsWith(this.text) ? fromEnd(old, 2) : 0
    let offset = 0
    for (let i = 0; i < keep; i++) offset += old[i]!.token.raw.length
    let src = next.slice(offset)
    let tokens = md.lexer(src)
    let links = Object.keys(tokens.links).length ? JSON.stringify(tokens.links) : ""
    if (keep && links) {
      keep = 0
      src = next
      tokens = md.lexer(src)
      links = JSON.stringify(tokens.links)
    }
    this.exact = covers(tokens, src)
    // Equal raw text is not enough to reuse a block: marked may tokenize it
    // differently depending on what follows. Equal output is, and the block
    // takes the new token so later offsets count the text it now covers.
    const out = old.slice(0, keep)
    for (const t of tokens) {
      const prev = old[out.length]
      const block = renderBlock(t)
      if (prev?.html === block.html) {
        prev.token = t
        out.push(prev)
      } else out.push(block)
    }
    this.text = next
    this.blocks = out
    this.links = links
    return out
  }

  /** Highlighting still wanted. While streaming, the block being written is left out. */
  jobs(streaming: boolean): Job[] {
    const end = streaming ? fromEnd(this.blocks, 1) : this.blocks.length
    return this.blocks.slice(0, end).flatMap((b) => b.jobs.filter((j) => !highlighted.has(keyOf(j))))
  }

  /** Re-render blocks whose highlighting has arrived. */
  refresh(): Block[] {
    if (this.blocks.some(waiting)) this.blocks = this.blocks.map((b) => (waiting(b) ? renderBlock(b.token) : b))
    return this.blocks
  }
}

// ---- DOM -------------------------------------------------------------------

interface Shown {
  block: Block
  nodes: ChildNode[]
}

/** Replace only the blocks that changed. Every block with HTML has at least one node. */
function patch(root: HTMLElement, prev: Shown[], blocks: Block[]): Shown[] {
  const tpl = document.createElement("template")
  const next: Shown[] = []
  for (const block of blocks) {
    if (!block.html) continue
    const p = prev[next.length]
    if (p?.block === block) {
      next.push(p)
      continue
    }
    tpl.innerHTML = block.html
    const nodes = [...tpl.content.childNodes]
    root.insertBefore(tpl.content, p ? p.nodes[0]! : null)
    if (p) for (const n of p.nodes) n.remove()
    next.push({ block, nodes })
  }
  for (const p of prev.slice(next.length)) for (const n of p.nodes) n.remove()
  return next
}

async function writeClipboard(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text)
    return true
  } catch {
    // A plain-http origin (`alforria serve` on a LAN address) has no async clipboard.
    const ta = document.createElement("textarea")
    ta.value = text
    ta.style.cssText = "position:fixed;opacity:0"
    document.body.append(ta)
    ta.select()
    const ok = document.execCommand("copy")
    ta.remove()
    return ok
  }
}

const copied = new WeakMap<Element, ReturnType<typeof setTimeout>>()

function onCopy(e: MouseEvent) {
  const btn = (e.target as Element).closest("button.copy")
  const code = btn?.closest("pre")?.querySelector("code")?.textContent
  if (!btn || code == null) return
  void writeClipboard(code).then((ok) => {
    if (!ok) return
    btn.textContent = "Copied"
    clearTimeout(copied.get(btn))
    copied.set(
      btn,
      setTimeout(() => (btn.textContent = "Copy"), 1500),
    )
  })
}

/** Assistant text. Cheap to re-render as `text` grows: settled blocks are left alone. */
export function Markdown(props: { text: string; streaming?: boolean }): JSX.Element {
  let root!: HTMLDivElement
  const stream = new MarkdownStream()
  let shown: Shown[] = []
  let timer: ReturnType<typeof setTimeout> | undefined
  let alive = true
  onCleanup(() => {
    alive = false
    clearTimeout(timer)
  })

  // Settled code highlights shortly after it appears. The block still being
  // written waits for the stream to end, so it never flickers between states.
  const schedule = (streaming: boolean) => {
    if (timer !== undefined || !stream.jobs(streaming).length) return
    timer = setTimeout(
      () => {
        timer = undefined
        void highlight(stream.jobs(!!props.streaming)).then(() => {
          if (!alive) return
          shown = patch(root, shown, stream.refresh())
          schedule(!!props.streaming)
        })
      },
      streaming ? 250 : 0,
    )
  }

  createEffect(() => {
    const streaming = !!props.streaming
    shown = patch(root, shown, stream.update(props.text))
    schedule(streaming)
  })

  // A native listener: delegation would touch `window` when this module loads.
  return <div ref={root} class="prose md" on:click={onCopy} />
}

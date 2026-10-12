// What the composer sends: request bodies for a prompt or a slash command,
// built from the draft. Pure, so the wire shapes are tested on their own.
import type { PromptInput, PromptPart } from "../../api/client"
import type { UserMessage } from "../../api/types"
import type { State } from "../../store/state"

export interface Attachment {
  name: string
  mime: string
  url: string
}

/** An `@` mention being typed: anything up to the caret that isn't whitespace. */
export const MENTION = /(^|\s)@(\S*)$/

/** `file://` URL for a project-relative path, each segment percent-encoded (the server decodes it). */
export function fileUrl(directory: string, path: string) {
  const abs = `${directory.replace(/\/$/, "")}/${path}`
  return `file://${abs.split("/").map(encodeURIComponent).join("/")}`
}

/** File parts for the `@path` mentions still present in the text, then the attachments. */
export function fileParts(text: string, files: string[], attachments: Attachment[], directory: string): PromptPart[] {
  const parts: PromptPart[] = []
  for (const f of files)
    if (text.includes(`@${f}`))
      parts.push({ type: "file", mime: "text/plain", filename: f, url: fileUrl(directory, f) })
  for (const a of attachments) parts.push({ type: "file", mime: a.mime, filename: a.name, url: a.url })
  return parts
}

export interface Draft {
  text: string
  files: string[]
  attachments: Attachment[]
  directory: string
  agent: string
  /** `providerID/modelID`. */
  model?: string
  variant?: string
}

const modelRef = (m?: string) =>
  m ? { providerID: m.split("/")[0]!, modelID: m.split("/").slice(1).join("/") } : undefined

export function promptBody(d: Draft): PromptInput {
  return {
    parts: [{ type: "text", text: d.text }, ...fileParts(d.text, d.files, d.attachments, d.directory)],
    agent: d.agent,
    model: modelRef(d.model),
    ...(d.variant ? { variant: d.variant } : {}),
  }
}

/** `/name args`: the command endpoint takes the model as one string and file parts beside the arguments. */
export function commandBody(d: Draft, command: string, args: string) {
  const parts = fileParts(d.text, d.files, d.attachments, d.directory) as (PromptPart & {
    type: "file"
    mime: string
    url: string
  })[]
  return {
    command,
    arguments: args,
    agent: d.agent,
    model: d.model,
    ...(d.variant ? { variant: d.variant } : {}),
    ...(parts.length ? { parts } : {}),
  }
}

/** The model (and variant) the session's last prompt used, as TS restores it. */
export function lastUserModel(s: State, sessionID: string) {
  const ids = s.messages[sessionID] ?? []
  for (let i = ids.length - 1; i >= 0; i--) {
    const m = s.message[ids[i]!]
    if (m?.role === "user") return (m as UserMessage).model
  }
}

// Attachments: images and PDFs go as themselves; text goes as text/plain so
// the server inlines it like a read. Anything else (archives, binaries) is
// refused rather than sent as garbled "text".
const TEXT_MIME =
  /^(text\/|application\/(json|xml|javascript|typescript|x-sh|x-yaml|yaml|toml|x-toml|sql|graphql|ld\+json))/
const IMAGE_MIME = /^image\/(png|jpeg|gif|webp)$/
const TEXT_EXT =
  /\.(txt|md|markdown|rst|csv|tsv|log|json|jsonc|ya?ml|toml|ini|cfg|conf|env|xml|html?|css|scss|less|svg|[cm]?[jt]sx?|py|rb|go|rs|java|kt|swift|c|cc|cpp|h|hpp|cs|php|sh|bash|zsh|fish|ps1|sql|graphql|proto|lua|pl|r|scala|clj|ex|exs|erl|hs|ml|nix|tf|vue|svelte|astro|gradle|lock|diff|patch)$|(^|\/)(Dockerfile|Makefile|LICENSE|README|\.gitignore|\.editorconfig)$/i

export type AttachmentKind = "image" | "pdf" | "text" | "maybe-text"

/** How to send a file, or undefined to refuse it. "maybe-text" needs a look at its bytes. */
export function attachmentKind(name: string, mime: string): AttachmentKind | undefined {
  if (IMAGE_MIME.test(mime)) return "image"
  if (mime === "application/pdf") return "pdf"
  if (TEXT_MIME.test(mime) || TEXT_EXT.test(name)) return "text"
  if (!mime || mime === "application/octet-stream") return "maybe-text"
}

/** A NUL in the first 8 KB marks a binary, as git and grep decide. */
export const looksBinary = (bytes: Uint8Array) => bytes.subarray(0, 8192).includes(0)

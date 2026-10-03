// Unified-diff rendering for approvals, tool previews and the Changes view.
// In slips, long lines soft-wrap with a hanging indent and break only at token
// boundaries, so a wrapped line never invents code (DESIGN.md: Mid-Token Wrap).
import { For, type JSX } from "solid-js"

export type DiffRow = { kind: "hunk"; text: string } | { kind: "add" | "del" | "ctx"; line: number; text: string }

/** Parse a unified diff (as `createTwoFilesPatch` emits) into rows. */
export function parseUnifiedDiff(patch: string): DiffRow[] {
  const rows: DiffRow[] = []
  let oldLine = 0
  let newLine = 0
  let inHunk = false
  for (const raw of patch.split("\n")) {
    if (raw.startsWith("@@")) {
      const m = /@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/.exec(raw)
      oldLine = m ? Number(m[1]) : 0
      newLine = m ? Number(m[2]) : 0
      rows.push({ kind: "hunk", text: raw })
      inHunk = true
      continue
    }
    if (!inHunk || raw.startsWith("\\")) continue
    const sign = raw[0]
    const text = raw.slice(1)
    if (sign === "+") rows.push({ kind: "add", line: newLine++, text })
    else if (sign === "-") rows.push({ kind: "del", line: oldLine++, text })
    else if (sign === " ") {
      rows.push({ kind: "ctx", line: newLine++, text })
      oldLine++
    } else if (raw === "") continue
    else inHunk = false
  }
  return rows
}

export function diffStats(rows: DiffRow[]) {
  let add = 0
  let del = 0
  for (const r of rows) {
    if (r.kind === "add") add++
    else if (r.kind === "del") del++
  }
  return { add, del }
}

/** Split code so it may only wrap after `.` `::` `(` `,` or around operators. */
export function breakable(code: string): JSX.Element {
  const out: JSX.Element[] = []
  const re = /(::|\.|\(|,)|( (?:=|<|>|-|\+|\|\||&&) )/g
  let last = 0
  for (let m = re.exec(code); m; m = re.exec(code)) {
    const end = m.index + m[0].length
    if (m[2]) {
      out.push(code.slice(last, m.index + 1), <wbr />, code.slice(m.index + 1, end))
    } else {
      out.push(code.slice(last, end), <wbr />)
    }
    last = end
  }
  out.push(code.slice(last))
  return out
}

export function Diff(props: { rows: DiffRow[] }) {
  return (
    <div class="diff">
      <For each={props.rows}>
        {(r) =>
          r.kind === "hunk" ? (
            <div class="ln hunk">{r.text}</div>
          ) : (
            <div class={`ln ${r.kind}`}>
              <span>{r.line}</span>
              <span>{r.kind === "add" ? "+" : r.kind === "del" ? "−" : " "}</span>
              <span class="src" style={{ "--ind": String(r.text.match(/^\s*/)![0].replace(/\t/g, "    ").length) }}>
                {breakable(r.text.trimStart())}
              </span>
            </div>
          )
        }
      </For>
    </div>
  )
}

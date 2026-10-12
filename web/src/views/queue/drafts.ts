// Half-answered questions, per request: the queue slip and the inline slip in
// a transcript render the same request, so they edit one draft. A draft lives
// until its request leaves the pending lists.
import { createEffect, createRoot } from "solid-js"
import { createStore, produce } from "solid-js/store"
import type { QuestionRequest } from "../../api/types"
import { interrupts } from "../../fleet/fleet"

export interface QuestionDraft {
  /** Selected option labels, per question. */
  answers: string[][]
  /** Free text, per question. */
  custom: string[]
}

const [drafts, setDrafts] = createStore<Record<string, QuestionDraft>>({})

createRoot(() =>
  createEffect(() => {
    const live = new Set(interrupts().map((i) => i.id))
    for (const id of Object.keys(drafts)) if (!live.has(id)) setDrafts(produce((d) => delete d[id]))
  }),
)

/** The answers a draft sends: for a single choice, typed text replaces the pick. */
export function finalAnswers(q: QuestionRequest, d: QuestionDraft) {
  return d.answers.map((sel, i) => {
    const text = d.custom[i]?.trim()
    if (!text) return sel
    return q.questions[i]?.multiple ? [...sel, text] : [text]
  })
}

/** The shared draft for one question request, and its edits. */
export function questionDraft(q: () => QuestionRequest) {
  const id = () => q().id
  const draft = (): QuestionDraft =>
    drafts[id()] ?? { answers: q().questions.map(() => []), custom: q().questions.map(() => "") }
  const edit = (fn: (d: QuestionDraft) => void) => {
    if (!drafts[id()]) setDrafts(id(), draft())
    setDrafts(id(), produce(fn))
  }
  return {
    draft,
    /** Pick an option; a single choice drops any typed text. */
    toggle(qi: number, label: string) {
      const multiple = !!q().questions[qi]?.multiple
      edit((d) => {
        const sel = d.answers[qi] ?? []
        if (!multiple) {
          d.answers[qi] = [label]
          d.custom[qi] = ""
        } else d.answers[qi] = sel.includes(label) ? sel.filter((l) => l !== label) : [...sel, label]
      })
    },
    /** Type an answer; for a single choice it replaces the picked option. */
    type(qi: number, text: string) {
      const multiple = !!q().questions[qi]?.multiple
      edit((d) => {
        d.custom[qi] = text
        if (!multiple && text.trim()) d.answers[qi] = []
      })
    },
    final: () => finalAnswers(q(), draft()),
  }
}

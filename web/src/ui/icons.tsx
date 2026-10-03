// Icon set: authored 1.5-stroke SVG, square caps, 16×16 (DESIGN.md: one
// stroke weight, no glyph icons). Paths are static trusted strings.
import type { JSX } from "solid-js"

const PATHS = {
  search: `<circle cx="7" cy="7" r="4.25"/><path d="M10.25 10.25 14 14"/>`,
  plus: `<path d="M8 2.5v11M2.5 8h11"/>`,
  check: `<path d="M2.75 8.5 6.25 12l7-8"/>`,
  external: `<path d="M9.25 2.75h4v4M13.25 2.75 7.5 8.5"/><path d="M11.25 9.5v3.75h-8.5v-8.5H6.5"/>`,
  close: `<path d="M3.75 3.75l8.5 8.5M12.25 3.75l-8.5 8.5"/>`,
  "chev-right": `<path d="M6 3.5 10.5 8 6 12.5"/>`,
  "chev-down": `<path d="M3.5 6 8 10.5 12.5 6"/>`,
  terminal: `<rect x="1.75" y="2.75" width="12.5" height="10.5"/><path d="M4.5 6.25 6.75 8 4.5 9.75M8.5 10h3"/>`,
  diff: `<rect x="2.75" y="1.75" width="10.5" height="12.5"/><path d="M8 4.25v4M6 6.25h4M6 11h4"/>`,
  file: `<path d="M3.25 1.75h6l3.5 3.5v9H3.25z"/><path d="M9.25 1.75v3.5h3.5"/>`,
  folder: `<path d="M1.75 3.25h4.5l1.5 1.5h6.5v8.5H1.75z"/>`,
  sliders: `<path d="M1.75 4h12.5M1.75 8h12.5M1.75 12h12.5"/><rect x="9.25" y="2.5" width="2.5" height="3" fill="currentColor" stroke="none"/><rect x="3.75" y="6.5" width="2.5" height="3" fill="currentColor" stroke="none"/><rect x="7.25" y="10.5" width="2.5" height="3" fill="currentColor" stroke="none"/>`,
  contrast: `<circle cx="8" cy="8" r="5.75"/><path d="M8 2.25a5.75 5.75 0 0 1 0 11.5z" fill="currentColor" stroke="none"/>`,
  fork: `<circle cx="4.5" cy="3.5" r="1.5"/><circle cx="4.5" cy="12.5" r="1.5"/><circle cx="11.5" cy="4.5" r="1.5"/><path d="M4.5 5v6M11.5 6v1.25c0 2-2 2.5-7 3.25"/>`,
  stop: `<rect x="4.25" y="4.25" width="7.5" height="7.5" fill="currentColor" stroke="none"/>`,
  send: `<path d="M8 13.25V3M3.75 7.25 8 3l4.25 4.25"/>`,
  attach: `<path d="M11.75 7.25 7.1 11.9a2.6 2.6 0 0 1-3.7-3.7L9 2.6a1.8 1.8 0 0 1 2.55 2.55L6.1 10.6"/>`,
  panel: `<rect x="1.75" y="2.75" width="12.5" height="10.5"/><path d="M6.25 2.75v10.5"/>`,
  "arrow-down": `<path d="M8 2.75v10.5M3.75 9 8 13.25 12.25 9"/>`,
  back: `<path d="M13.25 8H2.75M7 3.75 2.75 8 7 12.25"/>`,
  undo: `<path d="M5.5 3 2.5 6l3 3"/><path d="M2.5 6h7.25a3.75 3.75 0 0 1 0 7.5H7"/>`,
  copy: `<rect x="5.25" y="5.25" width="8.5" height="8.5"/><path d="M10.75 5.25V2.25h-8.5v8.5h3"/>`,
  rows: `<path d="M1.75 3.5h12.5M1.75 8h12.5M1.75 12.5h12.5"/>`,
  columns: `<rect x="1.75" y="2.75" width="12.5" height="10.5"/><path d="M6 2.75v10.5M10 2.75v10.5"/>`,
  compact: `<path d="M2.75 3.5h10.5M4.75 8h6.5M6.75 12.5h2.5"/>`,
  server: `<rect x="1.75" y="2.25" width="12.5" height="5"/><rect x="1.75" y="8.75" width="12.5" height="5"/><path d="M4.5 4.75h1M4.5 11.25h1"/>`,
  queue: `<rect x="1.75" y="1.75" width="12.5" height="4.5"/><path d="M1.75 9h12.5M1.75 12.5h12.5"/>`,
} as const

export type IconName = keyof typeof PATHS

export function Icon(props: { name: IconName; class?: string; size?: number }): JSX.Element {
  return (
    <svg
      class={`i ${props.class ?? ""}`}
      viewBox="0 0 16 16"
      width={props.size}
      height={props.size}
      aria-hidden="true"
      innerHTML={PATHS[props.name]}
    />
  )
}

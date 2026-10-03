// Theme: dark by default (long supervision), light matches the printed
// datasheet. index.html's preload script applies the stored value before
// first paint; this keeps it in sync afterwards.
import { createRoot, createSignal } from "solid-js"

export type Theme = "dark" | "light"
const KEY = "alforria.theme"

function stored(): Theme {
  try {
    const t = localStorage.getItem(KEY)
    if (t === "light" || t === "dark") return t
  } catch {
    /* storage unavailable: fall through to the default */
  }
  return "dark"
}

const [theme, set] = createRoot(() => createSignal<Theme>(typeof window === "undefined" ? "dark" : stored()))
export { theme }

export function setTheme(t: Theme) {
  set(t)
  document.documentElement.dataset.theme = t
  document.querySelector('meta[name="theme-color"]')?.setAttribute("content", t === "dark" ? "#0B0E10" : "#F5F7F8")
  try {
    localStorage.setItem(KEY, t)
  } catch {
    /* preference just won't persist */
  }
}

export function toggleTheme() {
  setTheme(theme() === "dark" ? "light" : "dark")
}

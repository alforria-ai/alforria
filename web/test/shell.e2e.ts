// `bun test/shell.e2e.ts`: keyboard reach, queue order and dialogs, against a
// running dev pair: `bun run server` (the seeded fleet, fresh) and `bun run dev`.
// ALFORRIA_UI points at the UI (default http://127.0.0.1:4610). It answers the
// seeded question and starts a session, so restart the server before running
// it again.
import { chromium, type Page } from "playwright"
import { readFileSync } from "node:fs"
import { join } from "node:path"

const UI = (process.env.ALFORRIA_UI ?? "http://127.0.0.1:4610").replace(/\/$/, "")
const AXE = readFileSync(join(import.meta.dir, "../node_modules/axe-core/axe.min.js"), "utf8")

let failures = 0
function check(ok: unknown, label: string, detail?: unknown) {
  if (ok) console.log(`  ✓ ${label}`)
  else {
    failures++
    console.log(`  ✗ ${label}${detail === undefined ? "" : ` — ${JSON.stringify(detail)}`}`)
  }
}

async function a11y(p: Page) {
  await p.addScriptTag({ content: AXE })
  return p.evaluate(async () => {
    const r = await (
      window as unknown as {
        axe: { run(d: Document): Promise<{ violations: { id: string; impact: string; nodes: unknown[] }[] }> }
      }
    ).axe.run(document)
    return r.violations
      .filter((v) => v.impact === "serious" || v.impact === "critical")
      .map((v) => `${v.id}×${v.nodes.length}`)
  })
}

const focused = (p: Page) =>
  p.evaluate(() => {
    const el = document.activeElement as HTMLElement | null
    return el ? `${el.tagName.toLowerCase()}.${el.className}|${el.textContent?.trim().slice(0, 40)}` : ""
  })
const inDialog = (p: Page) => p.evaluate(() => !!document.activeElement?.closest(".dialog"))

/** Tab cycles inside the open dialog and comes back to where it started. */
async function tabWraps(p: Page, name: string) {
  const start = await focused(p)
  const seen: string[] = []
  for (let i = 0; i < 30; i++) {
    await p.keyboard.press("Tab")
    if (!(await inDialog(p))) return check(false, `Tab stays inside the ${name}`, seen)
    const f = await focused(p)
    seen.push(f)
    if (f === start) return check(true, `Tab wraps inside the ${name} (${i + 1} stops)`)
  }
  check(false, `Tab wraps inside the ${name}`, seen)
}

const errors: string[] = []
const browser = await chromium.launch()
try {
  const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 }, bypassCSP: true })
  const p = await ctx.newPage()
  p.on("pageerror", (e) => errors.push(e.message))
  await p.goto(`${UI}/#/`)
  await p.locator("tr.s-row").first().waitFor()
  await p.waitForTimeout(800)

  console.log("queue")
  {
    // Ids are time-ordered behind their prefix, so the body is the age.
    const shown = (await p
      .locator(".q-list .slip")
      .evaluateAll((els) => els.map((e) => e.getAttribute("data-slip")))) as string[]
    const aged = [...shown].sort((a, b) => (a.slice(4) < b.slice(4) ? -1 : 1))
    check(
      shown.some((id) => id.startsWith("per_")) && shown.some((id) => id.startsWith("que_")),
      "the queue mixes permissions and questions",
    )
    check(JSON.stringify(shown) === JSON.stringify(aged), "the queue is in age order", shown)
  }

  console.log("overview Enter")
  {
    const seg = (label: string) => p.locator(".ws-head .seg button", { hasText: label })
    for (const label of ["Waiting", "Working", "Idle", "All"]) {
      await seg(label).focus()
      await p.keyboard.press("Enter")
      check((await seg(label).getAttribute("aria-pressed")) === "true", `Enter on the ${label} filter presses it`)
    }
    check(/#\/$/.test(p.url()), "Enter on a filter opens no session")

    await p.locator(".ws-folder").focus()
    await p.keyboard.press("Enter")
    await p.locator(".dialog.folder").waitFor()
    check(true, "Enter on Open folder opens the dialog")
    await p.waitForTimeout(400)
    await tabWraps(p, "folder dialog")
    await p.keyboard.press("Escape")

    await p.locator(".band .cmd").focus()
    await p.keyboard.press("Enter")
    await p.locator(".palette").waitFor()
    check(true, "Enter on the search field opens the palette")
    await tabWraps(p, "palette")
    await p.keyboard.press("Shift+Tab")
    check(
      await p.evaluate(() => !!document.activeElement?.classList.contains("palette-esc")),
      "Shift+Tab from the palette input wraps to its close button, never to a result",
    )
    await p.keyboard.press("Escape")

    await p.locator(".band .icon-btn.theme").focus()
    const theme = await p.evaluate(() => document.documentElement.dataset.theme)
    await p.keyboard.press("Enter")
    check(
      (await p.evaluate(() => document.documentElement.dataset.theme)) !== theme && /#\/$/.test(p.url()),
      "Enter on the theme button switches theme, nothing else",
    )
    await p.keyboard.press("Enter")

    await p.locator(".proj .add").first().focus()
    await p.keyboard.press("Enter")
    await p.waitForURL(/#\/focus\/ses_/)
    check(true, "Enter on a project's New session starts one")
    await p.goto(`${UI}/#/`)
    await p.locator("tr.s-row").first().waitFor()

    await p.locator("tr.s-row", { hasText: "Fix flaky config test" }).click()
    await p.waitForURL(/#\/focus\/ses_/)
    await p.goto(`${UI}/#/`)
    await p.locator("tr.s-row").first().waitFor()
    await p.locator("body").click({ position: { x: 700, y: 880 } })
    await p.keyboard.press("j")
    await p.keyboard.press("Enter")
    await p.waitForURL(/#\/focus\/ses_/)
    check(true, "Enter from the table opens the cursor row")
    await p.goto(`${UI}/#/`)
    await p.locator("tr.s-row").first().waitFor()
  }

  console.log("queue controls")
  {
    await p.keyboard.press("q")
    await p.waitForTimeout(700)
    const slips = () => p.locator(".q-list .slip")
    const before = await slips().count()
    // Enter on the session link follows the link; it does not stamp a verdict.
    const link = p.locator(".q-list .slip.is-active .session-link")
    await link.focus()
    await p.keyboard.press("Enter")
    await p.waitForURL(/#\/focus\/ses_/)
    check(true, "Enter on a slip's session link opens the session")
    await p.goto(`${UI}/#/`)
    await p.locator(".q-list .slip").first().waitFor()
    await p.waitForTimeout(800)
    check((await slips().count()) === before, "…and replies to nothing")

    const q = p.locator(".q-list .slip", { hasText: "Question" }).first()
    await q.click()
    await p.waitForTimeout(700)
    const option = p.locator(".q-list .slip.is-active .option").first()
    await option.focus()
    await p.keyboard.press("Enter")
    check((await option.getAttribute("aria-checked")) === "true", "Enter on an option picks it")
    await p.waitForTimeout(600)
    check((await slips().count()) === before && !(await p.locator(".verdict-stamp").count()), "…and doesn't submit")

    // Typed text replaces the single choice: one answer goes out.
    await p.locator(".q-list .slip.is-active .other-input").fill("Something else entirely")
    check((await option.getAttribute("aria-checked")) === "false", "typing an answer clears the picked option")
    // The transcript's inline slip edits the same draft.
    await p.locator(".q-list .slip.is-active .session-link").click()
    const inline = p.locator(".col.is-active .slip.inline .other-input")
    await inline.waitFor()
    check((await inline.inputValue()) === "Something else entirely", "the inline slip shows the queue's draft")
    await p.keyboard.press("Escape")
    await p.locator(".q-list .slip", { hasText: "Question" }).first().locator(".slip-head").click()
    await p.waitForTimeout(700)
    const reply = p.waitForRequest((r) => /\/question\/[^/]+\/reply/.test(r.url()))
    await p.locator(".q-list .slip.is-active .stamp.primary").click()
    const body = (await reply).postDataJSON() as { answers: string[][] }
    check(
      JSON.stringify(body.answers) === JSON.stringify([["Something else entirely"]]),
      "a question with custom text sends one answer",
      body,
    )
    await p.waitForTimeout(1200)
  }

  console.log("session list")
  {
    await p.locator("tr.s-row", { hasText: "Fix flaky config test" }).click()
    await p.locator(".col").first().waitFor()
    const stops = await p.locator(".rail-list [tabindex='0']").count()
    check(stops === 1, "the session list is one tab stop", stops)
    check(
      !(await p.locator(".rail-list [role=option] :is(button, a, input, [tabindex])").count()),
      "no controls inside an option",
    )
    await p.locator(".rail-list [tabindex='0']").focus()
    await p.keyboard.press("Home")
    const from = await p.evaluate(() => (document.activeElement as HTMLElement).dataset.rail)
    await p.keyboard.press("ArrowDown")
    const to = await p.evaluate(() => (document.activeElement as HTMLElement).dataset.rail)
    check(to && to !== from, "↓ moves to the next session")
    check(
      (await p.locator(".rail-list [tabindex='0']").getAttribute("data-rail")) === to,
      "…and the tab stop follows it",
    )
    await p.keyboard.press("Enter")
    await p.waitForURL(new RegExp(`#/focus/${to}`))
    check(true, "Enter opens it in the active column")
    await p.keyboard.press("Escape")
  }

  console.log("a11y")
  for (const theme of ["dark", "light"]) {
    await p.evaluate((t) => (document.documentElement.dataset.theme = t), theme)
    await p.goto(`${UI}/#/`)
    await p.locator("tr.s-row").first().waitFor()
    await p.evaluate((t) => (document.documentElement.dataset.theme = t), theme)
    // With the keyboard cursor on a row (plate-2).
    await p.locator("body").click({ position: { x: 700, y: 880 } })
    await p.keyboard.press("j")
    const v = await a11y(p)
    check(!v.length, `overview, ${theme}: no serious a11y violations`, v)
    await p.keyboard.press("Control+k")
    await p.locator(".palette").waitFor()
    const pal = await a11y(p)
    check(!pal.length, `palette, ${theme}: no serious a11y violations`, pal)
    await p.keyboard.press("Escape")
    await p.locator("tr.s-row", { hasText: "Add cursor pagination" }).click()
    await p.locator(".col").first().waitFor()
    await p.waitForTimeout(500)
    const f = await a11y(p)
    check(!f.length, `focus, ${theme}: no serious a11y violations`, f)
    await p.locator(".col.is-active .tabs button", { hasText: "Changes" }).click()
    await p.locator(".col.is-active .diff").first().waitFor({ timeout: 10_000 })
    const c = await a11y(p)
    check(!c.length, `changes, ${theme}: no serious a11y violations`, c)
    for (const page of ["providers", "permissions"]) {
      await p.goto(`${UI}/#/settings/${page}`)
      await p.waitForTimeout(800)
      await p.evaluate((t) => (document.documentElement.dataset.theme = t), theme)
      const s = await a11y(p)
      check(!s.length, `settings ${page}, ${theme}: no serious a11y violations`, s)
    }
  }
  await ctx.close()

  console.log("phone")
  {
    const phone = await browser.newContext({ viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true })
    const m = await phone.newPage()
    m.on("pageerror", (e) => errors.push(e.message))
    /** Every visible control under 40px either way. */
    const small = () =>
      m.evaluate(() =>
        Array.from(
          document.querySelectorAll<HTMLElement>("button, a[href], input, select, textarea, summary, [role=option]"),
        )
          .filter((e) => {
            const r = e.getBoundingClientRect()
            const visible = r.width > 0 && r.height > 0 && getComputedStyle(e).visibility !== "hidden"
            return visible && (r.height < 40 || r.width < 40)
          })
          .map((e) => `${e.className || e.tagName.toLowerCase()} “${(e.textContent ?? "").trim().slice(0, 20)}”`),
      )
    const targets = async (where: string) => {
      const s = await small()
      check(!s.length, `phone ${where}: every control is at least 40px`, s)
    }
    await m.goto(`${UI}/#/`)
    await m.locator(".q-list .slip").first().waitFor()
    await m.waitForTimeout(800)
    await targets("queue")
    await m.locator(".mobile-tabs button", { hasText: "Sessions" }).tap()
    await targets("sessions")
    await m.locator("tr.s-row").first().tap()
    await m.locator(".col").waitFor()
    await m.waitForTimeout(600)
    await targets("focus")
    for (const page of ["providers", "permissions", "appearance"]) {
      await m.goto(`${UI}/#/settings/${page}`)
      await m.waitForTimeout(800)
      await targets(`settings ${page}`)
    }
    await m.goto(`${UI}/#/settings/server`)
    await m.waitForTimeout(800)
    check(
      await m.evaluate(() => {
        const r = document.querySelector('.settings nav [aria-current="true"]')!.getBoundingClientRect()
        return r.left >= 0 && r.right <= innerWidth
      }),
      "the settings strip scrolls to the current page",
    )
    await phone.close()
  }
  console.log("settings")
  {
    // Last: a save reloads every instance, which drops pending asks.
    const p = await (await browser.newContext({ viewport: { width: 1440, height: 900 } })).newPage()
    await p.goto(`${UI}/#/settings/permissions`)
    const row = p.locator(".spec.numbered .row", { has: p.locator(".n", { hasText: /^webfetch$/ }) })
    const ask = row.locator(".perm-seg button", { hasText: "ask" })
    await ask.waitFor()
    let patches = 0
    p.on("request", (r) => r.method() === "PATCH" && r.url().includes("/global/config") && patches++)
    await ask.click()
    await p.locator(".saved-note", { hasText: "Saved" }).waitFor({ timeout: 5000 })
    check(!(await p.locator(".toast").count()), "a save shows in place, not in a toast")
    await ask.click()
    await p.waitForTimeout(600)
    check(patches === 1, "pressing the current value saves nothing", patches)
  }
} finally {
  await browser.close()
}

for (const e of errors) console.log(`  ! ${e}`)
console.log(failures || errors.length ? `\n${failures} failed, ${errors.length} page errors` : "\nall passed")
process.exit(failures || errors.length ? 1 : 0)

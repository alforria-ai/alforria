// `bun run e2e`: the end-to-end gate. Starts its own isolated alforria server
// with the seeded demo fleet (script/dev-server.ts, on its own ports and state
// dir so a running `bun run server` is untouched), serves the production build
// with `vite preview`, and drives it in Chromium: desktop flows and the phone
// navigation contract. Exits non-zero on any failed check or page error.
import { chromium, type Page } from "playwright"
import { join } from "node:path"
import { mkdtempSync, readFileSync } from "node:fs"
import { tmpdir } from "node:os"

const WEB = join(import.meta.dir, "../..")
const API_PORT = Number(process.env.E2E_API_PORT ?? 4797)
const UI_PORT = Number(process.env.E2E_UI_PORT ?? 4711)
// E2E_EMBEDDED=1 tests the UI as `alforria serve` embeds it (after `bun run
// pack` + `cargo build -p alforria`): real CSP, real SPA fallback, no proxy.
const EMBEDDED = process.env.E2E_EMBEDDED === "1"
const UI = EMBEDDED ? `http://127.0.0.1:${API_PORT}` : `http://127.0.0.1:${UI_PORT}`

let pass = 0
let fail = 0
const errors: string[] = []
function check(ok: unknown, what: string) {
  if (ok) {
    pass++
    console.log(`  ✓ ${what}`)
  } else {
    fail++
    console.log(`  ✗ ${what}`)
  }
}

async function waitFor(url: string, ms = 60_000) {
  const end = Date.now() + ms
  while (Date.now() < end) {
    try {
      if ((await fetch(url)).ok) return
    } catch {
      /* not up yet */
    }
    await Bun.sleep(250)
  }
  throw new Error(`timed out waiting for ${url}`)
}

async function waitForLine(stream: ReadableStream<Uint8Array>, needle: string, ms = 120_000) {
  const reader = stream.getReader()
  const decoder = new TextDecoder()
  let text = ""
  const end = Date.now() + ms
  while (Date.now() < end) {
    const { value, done } = await reader.read()
    if (done) break
    text += decoder.decode(value)
    if (text.includes(needle)) {
      reader.releaseLock()
      return text
    }
  }
  throw new Error(`server never printed "${needle}":\n${text}`)
}

if (!EMBEDDED) {
  const build = Bun.spawnSync(["bun", "run", "build"], { cwd: WEB, stdout: "pipe", stderr: "pipe" })
  if (build.exitCode !== 0) throw new Error(`build failed:\n${build.stderr.toString()}`)
}

const devDir = mkdtempSync(join(tmpdir(), "alforria-e2e-"))
const server = Bun.spawn(["bun", "script/dev-server.ts", "--busy-seconds", "300"], {
  cwd: WEB,
  env: { ...process.env, ALFORRIA_PORT: String(API_PORT), ALFORRIA_DEV_DIR: devDir },
  stdout: "pipe",
  stderr: "inherit",
})
const preview = EMBEDDED
  ? undefined
  : Bun.spawn(["bunx", "vite", "preview", "--port", String(UI_PORT), "--strictPort"], {
      cwd: WEB,
      env: { ...process.env, ALFORRIA_URL: `http://127.0.0.1:${API_PORT}` },
      stdout: "ignore",
      stderr: "inherit",
    })

const AXE = readFileSync(join(WEB, "node_modules/axe-core/axe.min.js"), "utf8")
/** Serious and critical axe-core violations on the current page. */
async function a11y(p: Page) {
  await p.addScriptTag({ content: AXE })
  return p.evaluate(async () => {
    const r = await (
      window as unknown as { axe: { run(d: Document): Promise<{ violations: { id: string; impact: string }[] }> } }
    ).axe.run(document)
    return r.violations.filter((v) => v.impact === "serious" || v.impact === "critical").map((v) => v.id)
  })
}

const watch = (p: Page, tag: string) => {
  p.on("pageerror", (e) => errors.push(`${tag} pageerror: ${e.message}`))
  p.on("console", (m) => m.type() === "error" && errors.push(`${tag} console: ${m.text()}`))
}

try {
  await waitForLine(server.stdout, "Ctrl-C to stop")
  await waitFor(`${UI}/global/health`)
  const browser = await chromium.launch()

  console.log("desktop")
  {
    const p = await (await browser.newContext({ viewport: { width: 1440, height: 900 } })).newPage()
    watch(p, "desktop")
    const res = await p.goto(`${UI}/#/`)
    if (EMBEDDED) {
      const csp = res?.headers()["content-security-policy"] ?? ""
      check(/script-src 'self' 'wasm-unsafe-eval' 'sha256-/.test(csp), "embedded: CSP hashes the theme-preload script")
      const deep = await fetch(`${UI}/some/deep/link`)
      check(deep.ok && (await deep.text()).includes('id="root"'), "embedded: unknown paths fall back to index.html")
    }
    await p.locator("tr.s-row").first().waitFor()
    await p.waitForTimeout(800)
    const waiting = async () => Number(await p.locator(".wait-count .n").textContent())
    check((await p.locator("tr.s-row").count()) === 10, "overview lists the 10 seeded sessions")
    check((await waiting()) === 3, "band counts 3 waiting")
    check((await p.locator(".q-list .slip").count()) === 3, "queue holds 3 slips, oldest open")
    const overviewA11y = await a11y(p)
    check(!overviewA11y.length, `overview has no serious a11y violations ${overviewA11y.join(" ")}`)

    // Approve with the keyboard: needs the slip to have been readable for a
    // moment. A double-press 100 ms later must not approve the next item,
    // which has just slid in under the same finger.
    await p.keyboard.press("q")
    await p.waitForTimeout(600)
    await p.keyboard.press("a")
    await p.waitForTimeout(100)
    await p.keyboard.press("a")
    await p.waitForTimeout(100)
    check((await p.locator(".verdict-stamp b").first().textContent()) === "Allowed once", "A stamps the verdict")
    await p.waitForTimeout(1500)
    check((await waiting()) === 2, "a double-press approves exactly one item")

    // Answer the question: pick an option and submit.
    await p.locator(".q-list .slip", { hasText: "Question" }).first().click()
    await p.waitForTimeout(700)
    await p.locator(".q-list .slip.is-active .option").first().click()
    await p.locator(".q-list .slip.is-active .stamp.primary").click()
    await p.waitForTimeout(1500)
    check((await waiting()) === 1, "answering the question clears it")

    // Open a session; the URL and the session list follow.
    await p.locator("tr.s-row", { hasText: "Apply pending migrations" }).click()
    await p.locator(".col.is-active .msg-user").first().waitFor()
    check(/#\/focus\/ses_/.test(p.url()), "opening a session gives it a URL")
    check((await p.locator("#queue .rail-row").count()) === 10, "focus shows the live session list")
    const focusA11y = await a11y(p)
    check(!focusA11y.length, `focus has no serious a11y violations ${focusA11y.join(" ")}`)

    // Prompt round-trip through the scripted backend.
    await p.locator(".col.is-active textarea").fill("Summarize what changed.")
    await p.keyboard.press("Enter")
    await p.locator(".col.is-active .msg-user", { hasText: "Summarize what changed." }).waitFor()
    await p.waitForTimeout(2000)
    check((await p.locator(".col.is-active .msg-asst").count()) >= 2, "a sent prompt gets a reply")

    // An attachment travels as a file part and shows in the transcript.
    await p.locator('.col.is-active input[type="file"]').setInputFiles({
      name: "notes.txt",
      mimeType: "text/plain",
      buffer: Buffer.from("remember the milk\n"),
    })
    check((await p.locator(".col.is-active .composer .attach-chip").count()) === 1, "an attached file shows as a chip")
    await p.locator(".col.is-active textarea").fill("See the attached notes.")
    await p.keyboard.press("Enter")
    await p.locator(".col.is-active .msg-user", { hasText: "See the attached notes." }).waitFor()
    await p.waitForTimeout(1200)
    check(
      (await p.locator(".col.is-active .msg-user .attach-chip", { hasText: "notes.txt" }).count()) === 1,
      "the attachment is sent and shown in the transcript",
    )

    // Subagent opens beside its parent.
    await p.locator(".rail-row", { hasText: "Consolidate color tokens" }).click()
    await p.locator(".col.is-active .subtask .link-btn").first().click()
    await p.waitForTimeout(800)
    check((await p.locator(".col").count()) === 2, "a subagent opens beside its parent")

    // History: back returns to the previous columns; Esc to the overview.
    await p.goBack()
    await p.waitForTimeout(400)
    check((await p.locator(".col").count()) === 1, "browser Back restores the previous columns")
    await p.keyboard.press("Escape")
    await p.waitForTimeout(300)
    check((await p.locator("table.reg").count()) === 1, "Esc returns to the overview")
  }

  console.log("phone")
  {
    const ctx = await browser.newContext({ viewport: { width: 390, height: 844 }, isMobile: true, hasTouch: true })
    const p = await ctx.newPage()
    watch(p, "phone")
    const shown = () =>
      p.evaluate(() => {
        const visible = (sel: string) => {
          const el = document.querySelector(sel)
          return !!el && el.getBoundingClientRect().width > 0
        }
        return {
          // The Queue tab carries a waiting badge ("QUEUE 1"); keep the word.
          tab: (document.querySelector('.mobile-tabs [aria-current="true"]') as HTMLElement | null)?.innerText
            .trim()
            .replace(/\d+$/, "")
            .split(/\s/)[0],
          pane: visible(".col") ? "Focus" : visible("table.reg") ? "Sessions" : visible(".q-list") ? "Queue" : "?",
          wide: Math.round(document.querySelector(".col")?.getBoundingClientRect().width ?? 0),
        }
      })
    const agree = async (what: string) => {
      const s = await shown()
      check(s.tab?.toLowerCase() === s.pane.toLowerCase(), `${what}: tab ${s.tab} matches pane ${s.pane}`)
      return s
    }
    await p.goto(`${UI}/#/`)
    await p.locator(".q-list").waitFor()
    await agree("start")
    await p.locator(".mobile-tabs button", { hasText: "Sessions" }).tap()
    await agree("sessions tab")
    await p.locator("tr.s-row").first().tap()
    await p.locator(".col").waitFor()
    const s = await agree("open a session")
    check(s.wide === 390, "a session takes the full phone width")
    await p.locator(".col .back-btn").tap()
    await agree("← back")
    await p.goBack()
    await p.waitForTimeout(300)
    await agree("browser Back")
    await p.locator(".mobile-tabs button", { hasText: "Queue" }).tap()
    await agree("queue tab")
    await ctx.close()
  }

  await browser.close()
} finally {
  server.kill("SIGINT")
  preview?.kill()
  await server.exited
}

for (const e of errors) console.log(`  ! ${e}`)
console.log(`\n${pass} passed, ${fail} failed, ${errors.length} page errors`)
process.exit(fail || errors.length ? 1 : 0)

import { beforeAll, describe, expect, test } from "vitest"
import { highlight, highlighterRequested, markdownReady, MarkdownStream, renderMarkdown } from "../src/ui/markdown"

beforeAll(() => markdownReady)

/** Every tag name the renderer may emit. Anything else came from the input. */
const ALLOWED = new Set(
  "a p br strong em del code pre div span button h1 h2 h3 h4 h5 h6 ul ol li input blockquote hr table thead tbody tr th td".split(
    " ",
  ),
)
const tags = (html: string) => [...html.matchAll(/<\/?([a-zA-Z][\w-]*)/g)].map((m) => m[1]!.toLowerCase())
/** Attribute names of emitted tags. Values are always double-quoted, so the pattern skips over them. */
const attrs = (html: string) =>
  [...html.matchAll(/<[a-zA-Z][^>]*>/g)].flatMap((m) =>
    [...m[0].matchAll(/\s([\w-]+)(?:="[^"]*")?/g)].map((a) => a[1]!),
  )

const DOC = `# Plan

Intro paragraph with **bold**, *em*, ~~gone~~, \`inline <code>\` and a [link](https://example.com).
Bare www.example.com and <https://auto.example> autolinks, and Vec<String> generics.

Setext heading
--------------

| crate | lines | note |
|:------|------:|:----:|
| core  | 1,204 | ok   |
| llm   | 87%   | \`x\` |

- [x] done item
- [ ] open item
  - nested

1. first

   loose paragraph
2. second
   \`\`\`bash
   cargo test
   \`\`\`

> quoted line
lazy continuation

***

\`\`\`rust
fn main() {
    // say hi
    println!("hi & <bye>");
}
\`\`\`

<div onclick="x()">raw html</div>

Trailing paragraph &mdash; with an entity.
`

describe("sanitizing", () => {
  test("text without code never asks for the highlighter", async () => {
    const stream = new MarkdownStream()
    stream.update("Just **prose**, a [link](https://x.y) and a | table |\n|---|\n| 1 |")
    expect(stream.jobs(false)).toEqual([])
    await highlight(stream.jobs(false))
    renderMarkdown("```rust\nfn main() {}\n```")
    expect(highlighterRequested()).toBe(false)
  })

  test("script tags and event handlers are neutralized", () => {
    const inputs = [
      "<script>alert(1)</script>",
      "hello <script>alert(1)</script> world",
      '<img src=x onerror="alert(1)">',
      "inline <img src=x onerror=alert(1)> image",
      "<svg/onload=alert(1)>",
      "<iframe srcdoc='<script>alert(1)</script>'></iframe>",
      '<a href="javascript:alert(1)">x</a>',
      "| a |\n|---|\n| <img src=x onerror=alert(1)> |",
      "- <script>alert(1)</script>\n- `<script>`",
      "<pre>\n<img src=x onerror=alert(1)>\n</pre>",
      "a <script> &#60;img src=x onerror=alert(1)&#62; </script>",
      '![x" onerror="alert(1)](https://x.y/a.png)',
      '[x](https://x.y "t\\" onmouseover=\\"alert(1)")',
    ]
    for (const input of inputs) {
      const html = renderMarkdown(input)
      for (const t of tags(html)) expect(ALLOWED.has(t), `<${t}> from ${input}`).toBe(true)
      for (const a of attrs(html))
        expect(["href", "title", "target", "rel", "class", "type", "align", "start"]).toContain(a)
      expect(html).not.toMatch(/<(script|img|svg|iframe)/i)
    }
    expect(renderMarkdown("<script>alert(1)</script>")).toContain("&lt;script&gt;alert(1)&lt;/script&gt;")
  })

  test("only http(s) and mailto links are clickable", () => {
    const dead = [
      "[x](javascript:alert(1))",
      "[x](JaVaScRiPt:alert(1))",
      "[x](jav&#x61;script:alert(1))",
      "[x](  javascript:alert(1))",
      "<javascript:alert(1)>",
      "[x](data:text/html;base64,PHNjcmlwdD4=)",
      "[x](vbscript:msgbox)",
      "[x](src/main.rs)",
      "![x](javascript:alert(1))",
      "[x][r]\n\n[r]: javascript:alert(1)",
    ]
    for (const input of dead) expect(renderMarkdown(input), input).not.toContain("<a ")

    const live = renderMarkdown("[docs](https://example.com/a?b=1&c=2) and mail@example.com")
    expect(live).toContain(
      '<a href="https://example.com/a?b=1&amp;c=2" target="_blank" rel="noreferrer noopener">docs</a>',
    )
    expect(live).toContain('<a href="mailto:mail@example.com" target="_blank" rel="noreferrer noopener">')
    expect(renderMarkdown("see www.example.com")).toContain('href="http://www.example.com"')
  })

  test("raw HTML is shown as text, comments dropped, <br> kept", () => {
    expect(renderMarkdown("use Vec<String> here")).toContain("use Vec&lt;String&gt; here")
    expect(renderMarkdown("a <!-- hidden --> b")).toBe("<p>a  b</p>\n")
    expect(renderMarkdown("| a |\n|---|\n| one<br>two |")).toContain("one<br>two")
    expect(renderMarkdown("x &mdash; y &amp; z")).toBe("<p>x &mdash; y &amp; z</p>\n")
  })
})

describe("GFM", () => {
  test("tables render with alignment and mono numerals", () => {
    const html = renderMarkdown("| crate | lines |\n|:--|--:|\n| core | 1,204 |\n| llm | 87% |")
    expect(html).toContain('<div class="table"><table>')
    expect(html).toContain('<th align="left">crate</th><th align="right">lines</th>')
    expect(html).toContain('<td align="left">core</td><td align="right" class="num">1,204</td>')
    expect(html).toContain('<td align="right" class="num">87%</td>')
  })

  test("task lists, strikethrough and headings", () => {
    const html = renderMarkdown("## Title\n\n- [x] done\n- [ ] open\n\n~~old~~")
    expect(html).toContain("<h2>Title</h2>")
    expect(html).toContain('<li class="task"><input checked="" disabled="" type="checkbox"> done</li>')
    expect(html).toContain('<li class="task"><input disabled="" type="checkbox"> open</li>')
    expect(html).toContain("<del>old</del>")
  })
})

describe("code blocks", () => {
  test("fenced code renders the code-block structure, escaped", () => {
    expect(renderMarkdown('```rust\nlet s = "<b>&";\n```')).toBe(
      '<pre class="code-block"><div class="code-head"><span class="lang">rust</span>' +
        '<button type="button" class="copy">Copy</button></div>' +
        "<code>let s = &quot;&lt;b&gt;&amp;&quot;;</code></pre>\n",
    )
    expect(renderMarkdown("```\nplain\n```")).toContain('<span class="lang">text</span>')
    expect(renderMarkdown('```"><script>\nx\n```')).toContain('<span class="lang">script</span>')
    expect(renderMarkdown("`a <b> &amp;`")).toBe("<p><code>a &lt;b&gt; &amp;amp;</code></p>\n")
  })

  test("only known languages queue highlighting, and streaming holds back the open block", () => {
    const stream = new MarkdownStream()
    stream.update("```klingon\nqapla'\n```\n\n```py\nx = 1\n```\n\nafter\n\n```ts\nlet a")
    expect(stream.jobs(false).map((j) => j.lang)).toEqual(["python", "typescript"])
    expect(stream.jobs(true).map((j) => j.lang)).toEqual(["python"])
  })

  test("shiki highlights into monochrome classes, preserving the code exactly", async () => {
    const src = 'pub fn main() {\n    // say hi\n    let s = "hi";\n}'
    const stream = new MarkdownStream()
    const before = stream.update("```rust\n" + src + "\n```\n")
    await highlight(stream.jobs(false))
    expect(highlighterRequested()).toBe(true)
    const after = stream.refresh()
    expect(after[0]).not.toBe(before[0])
    expect(stream.jobs(false)).toEqual([])

    const html = after[0]!.html
    expect(html).toContain('<span class="kw">pub fn</span>')
    expect(html).toContain('<span class="cm">// say hi</span>')
    expect(html).toContain('<span class="st">&quot;hi&quot;</span>')
    expect(html).not.toMatch(/style=|#0\d0\d0\d/)
    const code = /<code>([\s\S]*)<\/code>/.exec(html)![1]!
    const text = code
      .replace(/<[^>]+>/g, "")
      .replace(/&lt;/g, "<")
      .replace(/&gt;/g, ">")
      .replace(/&quot;/g, '"')
      .replace(/&#39;/g, "'")
      .replace(/&amp;/g, "&")
    expect(text).toBe(src)
    // Cached now: a fresh render of the same code is highlighted synchronously.
    expect(renderMarkdown("```rs\n" + src + "\n```")).toContain('<span class="kw">')
  })

  test("diff blocks take the diff tints, nothing else does", async () => {
    await highlight([{ lang: "diff", code: "@@ -1 +1 @@\n-old\n+new" }])
    const html = renderMarkdown("```diff\n@@ -1 +1 @@\n-old\n+new\n```")
    expect(html).toContain('<span class="dl">-old</span>\n<span class="ad">+new</span>')
    expect(html).toContain('<span class="cm">@@ -1 +1 @@</span>')
  })
})

describe("streaming", () => {
  test("every prefix renders exactly like a full render", () => {
    const stream = new MarkdownStream()
    for (let i = 0; i <= DOC.length; i++) {
      const html = stream
        .update(DOC.slice(0, i))
        .map((b) => b.html)
        .join("")
      expect(html, `prefix ${i}: ${JSON.stringify(DOC.slice(Math.max(0, i - 30), i))}`).toBe(
        renderMarkdown(DOC.slice(0, i)),
      )
    }
  })

  test("random streams of awkward fragments render like full renders", () => {
    // Constructs where marked's view of a block depends on what follows it, or
    // where its token `raw` stops matching the source.
    const frags = [
      "para\n-\n",
      "para\n--\n",
      "Title\n===\n",
      "a | b\n-|-\n1 | 2\n",
      "|a|b|\n|-|-|\n|1|2|\ntext\n",
      "| x |\n",
      "|---|\n",
      "- a\n\n  more\n\n- b\n",
      "* a\n* b\n\n\n* c\n",
      "- a\n+ b\n* c\n",
      "1. one\n2. two\n\n10. ten\n",
      "1) paren\n",
      "  - deep\n    - deeper\n",
      "- [ ] t\n- [x] u\n",
      "> quote\nlazy\n\n> again\n",
      "> - quoted list\n> more\n",
      "```\nunclosed code\n\nmore\n",
      "    indented code\n\n    more\n",
      "\tcode\n- \titem\n",
      "~~~\ntilde\n~~~\n",
      "para\n```js\ncode\n```\npara2\n",
      "1. step\n   ```sh\n   run\n   ```\n",
      "> ```py\n> x = 1\n> ```\n",
      "<div>\n*x*\n</div>\n",
      "<script>\nx\n</script>\n",
      "<!-- c -->\n",
      "<pre>\n\n</pre>\n",
      "[ref]: https://r.example\n",
      "[ref]\n",
      "**bold\nacross** lines\n",
      "`code` and ``a`b``\n",
      "line  \nhard break\n",
      "a\\\nb\n",
      "* * *\n",
      "text\n***\n",
      "crlf\r\nline\r\n",
      "plain words ",
      "\n",
      "\n\n",
      "  \n",
    ]
    let seed = 7
    const rand = (n: number) => ((seed = (seed * 16807) % 2147483647) % n) | 0
    for (let run = 0; run < 300; run++) {
      let doc = ""
      for (let i = 3 + rand(10); i > 0; i--) doc += frags[rand(frags.length)]
      const stream = new MarkdownStream()
      const step = [1, 4, 16][run % 3]!
      for (let at = 0; at < doc.length;) {
        at = Math.min(doc.length, at + 1 + rand(step))
        const prefix = doc.slice(0, at)
        const html = stream
          .update(prefix)
          .map((b) => b.html)
          .join("")
        expect(html, JSON.stringify(prefix)).toBe(renderMarkdown(prefix))
      }
    }
  })

  test("reference definitions arriving late re-render earlier blocks", () => {
    const doc = "See [the docs][d].\n\nMore text.\n\n[d]: https://example.com\n"
    const stream = new MarkdownStream()
    for (let i = 0; i <= doc.length; i++) {
      expect(
        stream
          .update(doc.slice(0, i))
          .map((b) => b.html)
          .join(""),
      ).toBe(renderMarkdown(doc.slice(0, i)))
    }
    expect(renderMarkdown(doc)).toContain('<a href="https://example.com"')
  })

  test("settled blocks keep their identity as text grows", () => {
    const stream = new MarkdownStream()
    const first = stream.update("one\n\ntwo\n\nthree")
    const second = stream.update("one\n\ntwo\n\nthree and more")
    expect(second[0]).toBe(first[0])
    expect(second.at(-1)).not.toBe(first.at(-1))
  })

  test("non-append edits fall back to a full render", () => {
    const stream = new MarkdownStream()
    stream.update("alpha\n\nbeta")
    const html = stream
      .update("gamma\n\nbeta\r\nsoft")
      .map((b) => b.html)
      .join("")
    expect(html).toBe(renderMarkdown("gamma\n\nbeta\nsoft"))
  })
})

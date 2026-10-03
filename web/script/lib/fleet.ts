// The demo fleet `bun run server` seeds: three small git projects and the
// scripted conversations run in them. Each session names the state it is
// left in, so the UI has every kind of row to show: finished, waiting on a
// permission, waiting on a question, delegating to a subagent, errored and
// busy for about a minute.

import type { Frame, Script, Turn } from "./backend"

export type State = "idle" | "permission" | "question" | "busy" | "error"

export type SessionDef = {
  script: Script
  // Scripts for the subagent sessions its task calls spawn.
  children?: Script[]
  state: State
}

export type ProjectDef = {
  name: string
  files: Record<string, string>
  // Merged into the project's opencode.json next to the provider.
  config?: Record<string, unknown>
  sessions: SessionDef[]
}

let calls = 0
const call = (name: string, args: unknown): Frame => ({ tool_call: { id: `call_${++calls}`, name, arguments: args } })
const text = (value: string): Frame => ({ text: value })
const usage = (input: number, output: number) => ({ input, output })
const step = (...frames: Frame[]): Turn => ({
  frames: [...frames, { finish: { reason: "tool_calls", usage: usage(900, 60) } }],
})
const answer = (...frames: Frame[]): Turn => ({
  frames: [...frames, { finish: { reason: "stop", usage: usage(1400, 180) } }],
})

// Text streamed in small pieces with a stall before each: the session stays
// busy for about `seconds`.
function drip(body: string, seconds: number): Turn {
  const words = body.split(/(?<= )/)
  const pieces = 30
  const size = Math.ceil(words.length / pieces)
  const frames: Frame[] = []
  for (let i = 0; i < words.length; i += size) {
    frames.push({ sleep_ms: Math.round((seconds * 1000) / pieces) }, text(words.slice(i, i + size).join("")))
  }
  return { frames: [...frames, { finish: { reason: "stop", usage: usage(2100, 420) } }] }
}

const todo = (content: string, status: string, priority = "medium") => ({ content, status, priority })

export function fleet(busySeconds: number): ProjectDef[] {
  return [
    {
      name: "atlas-api",
      // Shell commands need approval here, so the migration session waits.
      config: { permission: { bash: "ask" } },
      files: {
        "README.md":
          "# atlas-api\n\nREST API for the Atlas inventory service.\n\n    bun run dev         # http://localhost:8787\n    bun run db:migrate  # apply pending migrations\n",
        "package.json":
          JSON.stringify(
            {
              name: "atlas-api",
              private: true,
              type: "module",
              scripts: {
                dev: "bun --watch src/server.ts",
                test: "bun test",
                "db:migrate": "echo 'applied 2 migrations: 0007_users_cursor_index, 0008_users_email_unique'",
              },
            },
            null,
            2,
          ) + "\n",
        "src/server.ts":
          'import { listUsers } from "./routes/users"\n\nexport default {\n  port: 8787,\n  fetch(req: Request) {\n    const url = new URL(req.url)\n    if (url.pathname === "/users") return listUsers(url)\n    return new Response("not found", { status: 404 })\n  },\n}\n',
        "src/routes/users.ts":
          'import { db } from "../db"\n\nexport async function listUsers(url: URL) {\n  const users = await db.query("select id, email, name from users order by id")\n  return Response.json(users)\n}\n',
        "src/db.ts":
          "export const db = {\n  async query(sql: string, params: unknown[] = []) {\n    // Replaced by the pooled client in production.\n    return [] as { id: number; email: string; name: string }[]\n  },\n}\n",
        "migrations/0007_users_cursor_index.sql": "create index users_id_cursor on users (id);\n",
        "migrations/0008_users_email_unique.sql":
          "alter table users add constraint users_email_unique unique (email);\n",
      },
      sessions: [
        {
          state: "idle",
          script: {
            prompt: "Add cursor pagination to GET /users (limit and cursor query params).",
            title: "Add cursor pagination to GET /users",
            turns: [
              step(
                call("todowrite", {
                  todos: [
                    todo("Read the users route", "in_progress", "high"),
                    todo("Add limit/cursor to listUsers", "pending", "high"),
                    todo("Return the next cursor", "pending"),
                  ],
                }),
                call("read", { filePath: "src/routes/users.ts" }),
              ),
              step(
                text("The route loads every row. I'll page by `id` with a capped `limit`."),
                call("edit", {
                  filePath: "src/routes/users.ts",
                  oldString:
                    '  const users = await db.query("select id, email, name from users order by id")\n  return Response.json(users)\n',
                  newString:
                    '  const limit = Math.min(Number(url.searchParams.get("limit") ?? 50), 200)\n  const cursor = Number(url.searchParams.get("cursor") ?? 0)\n  const users = await db.query(\n    "select id, email, name from users where id > $1 order by id limit $2",\n    [cursor, limit],\n  )\n  const next = users.length === limit ? users[users.length - 1]!.id : null\n  return Response.json({ users, next })\n',
                }),
              ),
              step(
                call("todowrite", {
                  todos: [
                    todo("Read the users route", "completed", "high"),
                    todo("Add limit/cursor to listUsers", "completed", "high"),
                    todo("Return the next cursor", "completed"),
                  ],
                }),
              ),
              answer(
                text(
                  "`GET /users` now pages by id:\n\n- `limit` defaults to 50 and is capped at 200\n- `cursor` is the last id of the previous page\n- the response is `{ users, next }`; `next` is `null` on the last page\n\nMigration `0007_users_cursor_index` already adds the index this query needs.",
                ),
              ),
            ],
          },
        },
        {
          state: "permission",
          script: {
            prompt: "Apply the pending database migrations.",
            title: "Apply pending migrations",
            turns: [
              step(
                text("Two migrations are pending. Running the migration script."),
                call("bash", { command: "bun run db:migrate" }),
              ),
              answer(text("Both migrations applied: `0007_users_cursor_index` and `0008_users_email_unique`.")),
            ],
          },
        },
        {
          state: "idle",
          script: {
            prompt: "Why does /users/ (trailing slash) return 404?",
            title: "Trailing slash 404 on /users",
            turns: [
              step(call("read", { filePath: "src/server.ts" })),
              answer(
                text(
                  'The router compares `url.pathname === "/users"` exactly, so `/users/` falls through to the 404. Normalize first:\n\n```ts\nconst path = url.pathname.replace(/\\/+$/, "") || "/"\n```',
                ),
              ),
            ],
          },
        },
      ],
    },
    {
      name: "lumen-web",
      files: {
        "README.md": "# lumen-web\n\nCustomer dashboard (Solid + Vite).\n",
        "package.json":
          JSON.stringify(
            {
              name: "lumen-web",
              private: true,
              type: "module",
              scripts: { dev: "vite", build: "vite build" },
              dependencies: { "solid-js": "^1.9.0" },
              devDependencies: { vite: "^6.3.0", "vite-plugin-solid": "^2.11.0" },
            },
            null,
            2,
          ) + "\n",
        "vite.config.ts":
          'import { defineConfig } from "vite"\nimport solid from "vite-plugin-solid"\n\nexport default defineConfig({\n  plugins: [solid()],\n  build: { target: "esnext", polyfillModulePreload: false },\n})\n',
        "src/theme/tokens.ts":
          'export const color = {\n  accent: "#3b82f6",\n  surface: "#ffffff",\n  text: "#111827",\n}\n',
        "src/components/Button.tsx":
          'export function Button(props: { label: string }) {\n  return <button style={{ background: "#3b82f6", color: "#fff" }}>{props.label}</button>\n}\n',
        "src/components/Card.tsx":
          'import { color } from "../theme/tokens"\n\nexport function Card(props: { title: string }) {\n  return <section style={{ background: color.surface, border: "1px solid #e5e7eb" }}>{props.title}</section>\n}\n',
      },
      sessions: [
        {
          state: "idle",
          script: {
            prompt: "Consolidate the color tokens: move hardcoded colors in components into src/theme/tokens.ts.",
            title: "Consolidate color tokens",
            turns: [
              step(
                text("I'll have an explore agent find every hardcoded color first."),
                call("task", {
                  description: "Find hardcoded colors",
                  prompt:
                    "List every component under src/ that hardcodes a hex color instead of using src/theme/tokens.ts.",
                  subagent_type: "explore",
                }),
              ),
              answer(
                text(
                  "Two components bypass the tokens:\n\n| File | Hardcoded | Token |\n| --- | --- | --- |\n| `Button.tsx` | `#3b82f6` | `color.accent` |\n| `Card.tsx` | `#e5e7eb` | new `color.border` |\n\nWant me to apply the replacements?",
                ),
              ),
            ],
          },
          children: [
            {
              prompt: "List every component under src/ that hardcodes a hex color",
              title: "Find hardcoded colors",
              turns: [
                step(call("grep", { pattern: "#[0-9a-fA-F]{3,6}", path: "src/components" })),
                answer(
                  text(
                    "- src/components/Button.tsx: `#3b82f6` (matches `color.accent`), `#fff`\n- src/components/Card.tsx: `#e5e7eb` (no token yet)",
                  ),
                ),
              ],
            },
          ],
        },
        {
          state: "question",
          script: {
            prompt: "Add a usage chart to the dashboard.",
            title: "Usage chart on the dashboard",
            turns: [
              step(
                text("There's no chart library in the project yet."),
                call("question", {
                  questions: [
                    {
                      question: "Which chart library should the dashboard use?",
                      header: "Chart library",
                      options: [
                        { label: "uPlot", description: "Tiny and fast; time series only" },
                        { label: "ECharts", description: "Every chart type; ~300 kB" },
                        { label: "Hand-rolled SVG", description: "No dependency; one line chart" },
                      ],
                    },
                  ],
                }),
              ),
              answer(text("Noted. I'll add the chart with that library next.")),
            ],
          },
        },
        {
          state: "busy",
          script: {
            prompt: "Upgrade the build to Vite 7 and fix any breaking config changes.",
            title: "Upgrade the build to Vite 7",
            turns: [
              step(
                call("todowrite", {
                  todos: [
                    todo("Read vite.config.ts", "completed", "high"),
                    todo("Check plugin compatibility", "in_progress", "high"),
                    todo("Replace removed build options", "pending", "high"),
                    todo("Bump vite and vite-plugin-solid", "pending"),
                  ],
                }),
                call("read", { filePath: "vite.config.ts" }),
              ),
              drip(
                'Vite 7 drops Node 18 and changes the default browser target to `baseline-widely-available`, so `build.target: "esnext"` still works but is no longer needed. ' +
                  "`build.polyfillModulePreload` was removed in favour of `build.modulePreload.polyfill`; the config sets it to `false`, so it becomes `modulePreload: { polyfill: false }`. " +
                  "`vite-plugin-solid` 2.11 declares `vite ^7` as a peer, so it can move together with Vite. " +
                  "Nothing else in the config touches removed APIs. Next I'll bump both packages, update the option and run a production build to confirm the output is unchanged.",
                busySeconds,
              ),
            ],
          },
        },
      ],
    },
    {
      name: "ferrite-cli",
      files: {
        "README.md": "# ferrite\n\nA small CLI that syncs dotfiles between machines.\n",
        "Cargo.toml":
          '[package]\nname = "ferrite"\nversion = "0.4.2"\nedition = "2021"\n\n[dependencies]\nclap = { version = "4.5", features = ["derive"] }\nanyhow = "1"\n',
        "src/main.rs":
          'mod config;\n\nfn main() -> anyhow::Result<()> {\n    let config = config::load()?;\n    println!("syncing {} entries", config.entries.len());\n    Ok(())\n}\n',
        "src/config.rs":
          'use std::path::PathBuf;\n\npub struct Config {\n    pub entries: Vec<PathBuf>,\n}\n\npub fn path() -> PathBuf {\n    let home = std::env::var("HOME").expect("HOME");\n    PathBuf::from(home).join(".ferrite.toml")\n}\n\npub fn load() -> anyhow::Result<Config> {\n    let text = std::fs::read_to_string(path()).unwrap_or_default();\n    Ok(Config { entries: text.lines().map(PathBuf::from).collect() })\n}\n',
        "tests/config.rs":
          '#[test]\nfn loads_entries_from_home() {\n    std::env::set_var("HOME", "/tmp/ferrite-test");\n    std::fs::create_dir_all("/tmp/ferrite-test").unwrap();\n    std::fs::write("/tmp/ferrite-test/.ferrite.toml", "a\\nb\\n").unwrap();\n    assert_eq!(ferrite::config::load().unwrap().entries.len(), 2);\n}\n',
        ".env": "FERRITE_SYNC_TOKEN=fst_live_8f2c1d9e\nFERRITE_REMOTE=git@github.com:example/dotfiles.git\n",
      },
      sessions: [
        {
          state: "idle",
          script: {
            prompt: "tests/config.rs fails randomly in CI. Find out why and fix it.",
            title: "Fix flaky config test",
            turns: [
              step(call("read", { filePath: "src/config.rs" }), call("read", { filePath: "tests/config.rs" })),
              step(
                text(
                  "Every CI job on a runner shares the fixed `/tmp/ferrite-test` dir, so concurrent runs overwrite each other's config. I'll give each test process its own dir.",
                ),
                call("edit", {
                  filePath: "tests/config.rs",
                  oldString:
                    '    std::env::set_var("HOME", "/tmp/ferrite-test");\n    std::fs::create_dir_all("/tmp/ferrite-test").unwrap();\n    std::fs::write("/tmp/ferrite-test/.ferrite.toml", "a\\nb\\n").unwrap();\n',
                  newString:
                    '    let dir = std::env::temp_dir().join(format!("ferrite-{}", std::process::id()));\n    std::fs::create_dir_all(&dir).unwrap();\n    std::fs::write(dir.join(".ferrite.toml"), "a\\nb\\n").unwrap();\n    std::env::set_var("HOME", &dir);\n',
                }),
              ),
              answer(
                text(
                  "Fixed: the test now writes to a per-process temp dir, so parallel CI jobs no longer clobber each other. Longer term, `config::load` should take the config dir so tests stop mutating `HOME`.",
                ),
              ),
            ],
          },
        },
        {
          state: "permission",
          script: {
            prompt: "Check .env for secrets that should not be committed.",
            title: "Audit secrets in .env",
            turns: [
              step(call("read", { filePath: ".env" })),
              answer(
                text(
                  "`.env` holds a live sync token (`FERRITE_SYNC_TOKEN`). Add `.env` to `.gitignore` and rotate the token.",
                ),
              ),
            ],
          },
        },
        {
          state: "error",
          script: {
            prompt: "Bump clap to 5.0 and fix the derive changes.",
            title: "Bump clap to 5.0",
            turns: [
              { error: { status: 400, message: "Bad request: upstream provider rejected the request (req_7f3a)" } },
            ],
          },
        },
      ],
    },
  ]
}

// Every script the backend must know, subagent children included.
export function scripts(projects: ProjectDef[]): Script[] {
  return projects.flatMap((project) =>
    project.sessions.flatMap((session) => [session.script, ...(session.children ?? [])]),
  )
}

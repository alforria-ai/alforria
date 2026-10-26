import { defineConfig } from "vite"
import solid from "vite-plugin-solid"

// `bun run dev` proxies the API to a running server. `bun run server` starts an
// isolated one with a scripted model and prints its URL; point at it with
// ALFORRIA_URL (default: the dev harness's fixed port).
const target = process.env.ALFORRIA_URL ?? "http://127.0.0.1:4697"
// `alforria serve` wants a password; the dev harness uses a fixed one
// (script/lib/alforria.ts). Against another serve, pass the one it printed
// in ALFORRIA_PASSWORD (and ALFORRIA_USERNAME if it isn't "alforria").
const username = process.env.ALFORRIA_USERNAME ?? "alforria"
const password = process.env.ALFORRIA_PASSWORD ?? "alforria-dev"
const authorization = `Basic ${Buffer.from(`${username}:${password}`).toString("base64")}`
const api = [
  "/global",
  "/event",
  "/session",
  "/experimental",
  "/project",
  "/permission",
  "/question",
  "/path",
  "/config",
  "/provider",
  "/agent",
  "/command",
  "/find",
  "/file",
  "/pty",
  "/vcs",
  "/mcp",
  "/lsp",
  "/auth",
  "/workflow",
  "/api",
  "/experimental",
  "/doc",
  "/openapi.json",
]

// The server refuses state-changing requests and PTY tickets whose Origin is
// not its own (or a --cors origin), and answers only to loopback Host names
// (changeOrigin rewrites Host to the target). Vite's pages have vite's
// origin, so for requests *from vite's own pages* (Origin host ==
// Host header) present the target's origin instead. Anything else keeps its
// real Origin, so a foreign site can't use this proxy to get past the check.
type Req = { headers: Record<string, string | string[] | undefined> }
type ProxyReq = { setHeader(k: string, v: string): void; getHeader(k: string): unknown }
const fromOwnPage = (req: Req) => {
  const origin = req.headers.origin
  const host = req.headers.host
  if (typeof origin !== "string" || typeof host !== "string") return false
  try {
    return new URL(origin).host === host
  } catch {
    return false
  }
}
const proxy = Object.fromEntries(
  api.map((p) => [
    p,
    {
      target,
      changeOrigin: true,
      ws: p === "/pty",
      configure: (server: { on(event: string, cb: (proxyReq: ProxyReq, req: Req) => void): void }) => {
        const rewrite = (proxyReq: ProxyReq, req: Req) => {
          if (fromOwnPage(req)) proxyReq.setHeader("origin", target)
          // Present the server's credentials so the browser never prompts.
          if (!proxyReq.getHeader("authorization")) proxyReq.setHeader("authorization", authorization)
        }
        server.on("proxyReq", rewrite)
        server.on("proxyReqWs", rewrite)
      },
    },
  ]),
)

// The dev server stays on loopback: its proxy presents the server's origin
// and password, so whoever reaches it gets the agent. ALFORRIA_LAN=1 (the
// counterpart of `bun run server --lan`) opens it to the LAN for a phone;
// vite's own --host works too.
const lan = process.env.ALFORRIA_LAN === "1"

export default defineConfig({
  plugins: [solid()],
  server: {
    host: lan ? "0.0.0.0" : "127.0.0.1",
    port: 4610,
    proxy,
  },
  // `vite preview` serves the production build the same way (the e2e suite uses it).
  preview: {
    host: "127.0.0.1",
    proxy,
  },
  build: {
    target: "es2022",
    outDir: "dist",
    emptyOutDir: true,
    sourcemap: false,
    assetsInlineLimit: 0,
  },
  test: {
    environment: "node",
    include: ["test/**/*.test.ts"],
  },
})

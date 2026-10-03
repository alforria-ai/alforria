import { defineConfig } from "vite"
import solid from "vite-plugin-solid"

// `bun run dev` proxies the API to a running server. `bun run server` starts an
// isolated one with a scripted model and prints its URL; point at it with
// ALFORRIA_URL (default: the dev harness's fixed port).
const target = process.env.ALFORRIA_URL ?? "http://127.0.0.1:4697"
const api = [
  "/global",
  "/event",
  "/session",
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
  "/auth",
  "/api",
  "/doc",
  "/openapi.json",
]

// The server refuses state-changing requests and PTY tickets whose Origin is
// not its own host or localhost. A page served by vite to a phone on the LAN
// has vite's origin, so for requests *from vite's own pages* (Origin host ==
// Host header) present the target's origin instead. Anything else keeps its
// real Origin, so a foreign site can't use this proxy to get past the check.
type Req = { headers: Record<string, string | string[] | undefined> }
type ProxyReq = { setHeader(k: string, v: string): void }
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
        }
        server.on("proxyReq", rewrite)
        server.on("proxyReqWs", rewrite)
      },
    },
  ]),
)

export default defineConfig({
  plugins: [solid()],
  server: {
    host: "0.0.0.0",
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

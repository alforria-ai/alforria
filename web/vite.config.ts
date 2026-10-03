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

// The server checks the Origin of state-changing requests and PTY tickets
// against its own host and localhost. Through the proxy (e.g. a phone on the
// LAN hitting vite) the browser's origin is the proxy's, so present the
// target's origin instead.
const proxy = Object.fromEntries(
  api.map((p) => [
    p,
    {
      target,
      changeOrigin: true,
      ws: p === "/pty",
      configure: (server: {
        on(event: string, cb: (req: { setHeader(k: string, v: string): void }) => void): void
      }) => {
        server.on("proxyReq", (req) => req.setHeader("origin", target))
        server.on("proxyReqWs", (req) => req.setHeader("origin", target))
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

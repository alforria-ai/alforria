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
  "/doc",
  "/openapi.json",
]

export default defineConfig({
  plugins: [solid()],
  server: {
    host: "0.0.0.0",
    port: 4610,
    proxy: Object.fromEntries(api.map((p) => [p, { target, changeOrigin: true, ws: p === "/pty" }])),
  },
  // `vite preview` serves the production build the same way (the e2e suite uses it).
  preview: {
    host: "127.0.0.1",
    proxy: Object.fromEntries(api.map((p) => [p, { target, changeOrigin: true, ws: p === "/pty" }])),
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

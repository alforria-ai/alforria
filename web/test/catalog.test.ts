// Per-directory catalogs: models, MCP and LSP read per project directory,
// re-read when that directory's instance is disposed or its status changes.
import { beforeEach, describe, expect, test, vi } from "vitest"
import { createRoot } from "solid-js"
import type { GlobalEvent } from "../src/api/types"

const api = vi.hoisted(() => ({}) as Record<string, ReturnType<typeof import("vitest").vi.fn>>)
vi.mock("../src/api/client", async (original) => ({
  ...(await original<typeof import("../src/api/client")>()),
  api,
}))

const { loadCatalog, modelMap, modelsFor, useCatalog } = await import("../src/sync/catalog")
const { onFrame } = await import("../src/sync/sync")
const { flush, setState, state } = await import("../src/store/store")
const { emptyState } = await import("../src/store/state")

const providers = (dir: string) => ({
  providers: [{ id: "p", name: "P", models: { [dir]: { id: dir.slice(1), name: `model in ${dir}` } } }],
  default: {},
})
const frame = (type: string, properties: unknown, directory: string) =>
  ({ directory, payload: { id: "evt", type, properties } }) as GlobalEvent
const settle = async () => {
  for (let i = 0; i < 5; i++) await new Promise((r) => setTimeout(r, 0))
  flush()
}

beforeEach(() => {
  setState(emptyState())
  Object.assign(api, {
    providers: vi.fn(async (dir: string) => providers(dir)),
    mcpStatus: vi.fn(async () => ({ docs: { status: "connected" } })),
    lspStatus: vi.fn(async () => []),
    agents: vi.fn(async () => []),
    commands: vi.fn(async () => []),
  })
})

describe("catalogs", () => {
  test("each directory has its own models; unread ones fall back to the server-wide list", async () => {
    setState("models", { "p/default": { name: "default" } })
    const dispose = createRoot((dispose) => {
      useCatalog(() => "/a")
      return dispose
    })
    await loadCatalog("/b")
    await settle()
    expect(Object.keys(modelsFor("/a"))).toEqual(["p/a"])
    expect(Object.keys(modelsFor("/b"))).toEqual(["p/b"])
    expect(Object.keys(modelsFor("/c"))).toEqual(["p/default"])
    expect(state.catalogs["/a"]?.mcp).toEqual({ docs: { status: "connected" } })
    dispose()
  })

  test("status events and disposal re-read only that directory", async () => {
    await loadCatalog("/a")
    api.lspStatus!.mockResolvedValue([{ id: "ts", name: "typescript", root: "/a", status: "connected" }])
    onFrame(frame("lsp.updated", {}, "/a"))
    onFrame(frame("lsp.updated", {}, "/unread"))
    await settle()
    expect(state.catalogs["/a"]?.lsp).toHaveLength(1)
    expect(state.catalogs["/unread"]).toBeUndefined()
    const calls = api.providers!.mock.calls.length
    onFrame(frame("server.instance.disposed", { directory: "/a" }, "/a"))
    await settle()
    expect(api.providers!.mock.calls.length).toBe(calls + 1)
  })

  test("a model's reasoning variants, disabled ones left out", () => {
    expect(
      modelMap({
        providers: [
          {
            id: "p",
            name: "P",
            models: {
              a: { id: "a", name: "A", variants: { low: {}, high: {}, max: { disabled: true } } },
              b: { id: "b", name: "B", variants: {} },
              c: { id: "c", name: "C" },
            },
          },
        ],
        default: {},
      }),
    ).toEqual({
      "p/a": { name: "A", context: undefined, variants: ["low", "high"] },
      "p/b": { name: "B", context: undefined },
      "p/c": { name: "C", context: undefined },
    })
  })
})

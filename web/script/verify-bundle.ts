#!/usr/bin/env bun
// Compare extracted contents so tar/zstd versions do not create false drift.
import { $ } from "bun"
import { mkdtempSync, readdirSync, readFileSync, rmSync } from "node:fs"
import { tmpdir } from "node:os"
import path from "node:path"

const web = path.resolve(import.meta.dir, "..")
const unpacked = mkdtempSync(path.join(tmpdir(), "alforria-ui-check-"))
const files = (root: string, dir = ""): string[] =>
  readdirSync(path.join(root, dir), { withFileTypes: true }).flatMap((entry) => {
    const name = path.join(dir, entry.name)
    return entry.isDirectory() ? files(root, name) : [name]
  })

try {
  await $`tar -xf ${path.join(web, "../crates/alforria-ui/assets/ui.tar.zst")} -C ${unpacked}`
  const dist = path.join(web, "dist")
  const expected = files(dist)
    .filter((name) => !name.endsWith(".map"))
    .sort()
  const actual = files(unpacked).sort()
  const changed = new Set([...expected, ...actual].filter((name) => !expected.includes(name) || !actual.includes(name)))
  for (const name of expected) {
    if (actual.includes(name) && !readFileSync(path.join(dist, name)).equals(readFileSync(path.join(unpacked, name))))
      changed.add(name)
  }
  if (changed.size)
    throw new Error(
      `Embedded UI differs from web/dist: ${[...changed].join(", ")}. Run bun run build && bun run pack and commit the bundle.`,
    )
  console.log(`Embedded UI matches the build (${expected.length} files)`)
} finally {
  rmSync(unpacked, { recursive: true, force: true })
}

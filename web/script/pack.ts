#!/usr/bin/env bun
// Pack web/dist into the deterministic zstd tarball that crates/alforria-ui
// embeds (include_bytes!) and `alforria serve` decompresses once at startup.
// Sorted entries and zeroed mtime/owner: identical dist trees produce
// byte-identical artifacts, so the committed asset only changes when the UI does.
import { $ } from "bun"
import { existsSync, rmSync } from "node:fs"
import path from "node:path"

const web = path.resolve(import.meta.dir, "..")
const dist = path.join(web, "dist")
const out = path.resolve(web, "../crates/alforria-ui/assets/ui.tar.zst")

if (!existsSync(path.join(dist, "index.html"))) {
  console.error(`error: ${dist}/index.html not found — run \`bun run build\` first`)
  process.exit(1)
}

rmSync(out, { force: true })
await $`tar --zstd -cf ${out} -C ${dist} --sort=name --mtime=@0 --owner=0 --group=0 --numeric-owner --exclude=*.map .`
const bytes = Bun.file(out).size
console.log(`wrote ${path.relative(process.cwd(), out)} (${(bytes / 1024).toFixed(0)} KB)`)

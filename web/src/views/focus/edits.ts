// Pure helpers for the Changes view, apart from its JSX so tests can load them.

/**
 * The files one completed edit tool changed, with their patches. apply_patch
 * touches several: its `metadata.files[]` carries each one's own patch (and
 * `movePath` for a rename), while its `filepath` joins them for display.
 */
export function toolChanges(
  tool: string,
  input: Record<string, unknown>,
  meta: Record<string, unknown>,
): { file: string; patch: string }[] {
  if (tool === "apply_patch") {
    const files = Array.isArray(meta.files) ? (meta.files as Record<string, unknown>[]) : []
    return files.map((f) => ({
      file: String(f.movePath ?? f.filePath ?? f.relativePath ?? ""),
      patch: typeof f.patch === "string" ? f.patch : "",
    }))
  }
  const file = String(meta.filepath ?? input.filePath ?? "")
  let patch = typeof meta.diff === "string" ? meta.diff : ""
  if (!patch && tool === "write" && typeof input.content === "string") {
    const lines = input.content.replace(/\n$/, "").split("\n")
    patch = `@@ -0,0 +1,${lines.length} @@\n${lines.map((l) => `+${l}`).join("\n")}`
  }
  return [{ file, patch }]
}

//! Embedded alforria web UI.
//!
//! The UI is built in the `alforria-ai/web` repo (a fork of opencode's
//! `packages/app`), packed to a deterministic zstd tarball by `script/pack.ts`,
//! and committed at `assets/ui.tar.zst`. This crate embeds that tarball and
//! exposes it as an in-memory `path -> asset` map, decompressed once on first
//! use. The server adapts [`UiBundle`] into its `UiBackend` seam.

use std::collections::HashMap;
use std::io::Read;
use std::sync::OnceLock;

/// The committed UI bundle. Regenerate with `bun run pack` in `alforria-ai/web`
/// and copy the result here.
static UI_TARBALL: &[u8] = include_bytes!("../assets/ui.tar.zst");

/// One file served from the embedded UI map.
#[derive(Debug, Clone)]
pub struct Asset {
    /// Content type from the extension, matching TS `FSUtil.mimeType`.
    pub mime: &'static str,
    /// Raw file bytes.
    pub bytes: Vec<u8>,
}

/// The decompressed UI, keyed by path relative to the UI root (`index.html`,
/// `assets/index-*.js`, …). Paths never carry a leading `./` or `/`.
#[derive(Debug, Default)]
pub struct UiBundle {
    files: HashMap<String, Asset>,
}

impl UiBundle {
    /// Whether the bundle holds any files. A corrupt/missing artifact yields an
    /// empty bundle rather than failing the build.
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// Number of embedded files.
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// TS `embeddedWebUI[<path>]` — `path` may carry a leading slash.
    pub fn get(&self, path: &str) -> Option<&Asset> {
        self.files.get(path.strip_prefix('/').unwrap_or(path))
    }

    /// TS `embeddedWebUI["index.html"]` — the SPA fallback.
    pub fn index(&self) -> Option<&Asset> {
        self.files.get("index.html")
    }
}

/// The embedded bundle, decompressed once on first call.
pub fn bundle() -> &'static UiBundle {
    static BUNDLE: OnceLock<UiBundle> = OnceLock::new();
    BUNDLE.get_or_init(decompress)
}

fn decompress() -> UiBundle {
    let mut files = HashMap::new();
    let decoder = match zstd::stream::read::Decoder::new(UI_TARBALL) {
        Ok(decoder) => decoder,
        Err(error) => {
            tracing::error!("embedded web UI: zstd init failed: {error}");
            return UiBundle { files };
        }
    };
    let mut archive = tar::Archive::new(decoder);
    let entries = match archive.entries() {
        Ok(entries) => entries,
        Err(error) => {
            tracing::error!("embedded web UI: tar read failed: {error}");
            return UiBundle { files };
        }
    };
    for entry in entries {
        let mut entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::error!("embedded web UI: tar entry failed: {error}");
                return UiBundle { files };
            }
        };
        if entry.header().entry_type().is_dir() {
            continue;
        }
        let path = match entry.path() {
            Ok(path) => path.to_string_lossy().trim_start_matches("./").to_string(),
            Err(error) => {
                tracing::error!("embedded web UI: bad entry path: {error}");
                continue;
            }
        };
        if path.is_empty() {
            continue;
        }
        let mime = mime_for(&path);
        let mut bytes = Vec::new();
        if let Err(error) = entry.read_to_end(&mut bytes) {
            tracing::error!("embedded web UI: read {path} failed: {error}");
            continue;
        }
        files.insert(path, Asset { mime, bytes });
    }
    UiBundle { files }
}

/// Extension → MIME, mirroring `mime-types`' `lookup` for the extensions the
/// bundle actually contains, with the same `application/octet-stream` fallback
/// (`fs-util.ts:224-226`).
fn mime_for(path: &str) -> &'static str {
    let ext = path
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "html" | "htm" => "text/html",
        "js" | "mjs" => "text/javascript",
        "css" => "text/css",
        "json" | "map" => "application/json",
        "webmanifest" => "application/manifest+json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/vnd.microsoft.icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "aac" => "audio/aac",
        "mp4" => "video/mp4",
        "wasm" => "application/wasm",
        "txt" => "text/plain",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_is_populated_and_has_index() {
        let bundle = bundle();
        assert!(!bundle.is_empty(), "embedded UI bundle must not be empty");
        assert!(bundle.index().is_some(), "bundle must have index.html");
    }

    #[test]
    fn get_strips_leading_slash_and_index_falls_back() {
        let bundle = bundle();
        assert!(bundle.get("/index.html").is_some());
        assert_eq!(bundle.index().map(|asset| asset.mime), Some("text/html"));
    }

    #[test]
    fn mime_matches_mime_types_lookup() {
        assert_eq!(mime_for("assets/index-abc.js"), "text/javascript");
        assert_eq!(mime_for("assets/index-abc.css"), "text/css");
        assert_eq!(mime_for("site.webmanifest"), "application/manifest+json");
        assert_eq!(mime_for("_headers"), "application/octet-stream");
    }
}

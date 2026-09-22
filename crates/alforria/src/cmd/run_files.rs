//! run.ts:357-414 — the `--file` attachment resolution (local `file://`
//! parts; attach-mode `data:` URIs).

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::error::CliError;

/// `ATTACH_FILE_MAX_BYTES` (run.ts:59).
pub const ATTACH_FILE_MAX_BYTES: u64 = 10 * 1024 * 1024;

/// `pathToFileURL(path).href` (node:url) — `file://` + percent-encoded
/// absolute path.
pub fn path_to_file_url(path: &Path) -> String {
    const SAFE: &str = "-._~!$&'()*+,;=:@/";
    let mut out = String::from("file://");
    for byte in path.display().to_string().into_bytes() {
        let ch = byte as char;
        if ch.is_ascii_alphanumeric() || SAFE.contains(ch) {
            out.push(ch);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// `FSUtil.mimeType` (core/fs-util.ts:224-226): `mime-types` `lookup`
/// with an `application/octet-stream` fallback. The full mime-types table
/// is not ported — the common extensions cover the observable surface.
pub fn mime_type(path: &Path) -> &'static str {
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "aac" => "audio/aac",
        "avi" => "video/x-msvideo",
        "bz2" => "application/x-bzip2",
        "css" => "text/css",
        "csv" => "text/csv",
        "gif" => "image/gif",
        "gz" => "application/gzip",
        "htm" | "html" => "text/html",
        "ico" => "image/vnd.microsoft.icon",
        "jar" => "application/java-archive",
        "jpeg" | "jpg" => "image/jpeg",
        "js" => "text/javascript",
        "json" => "application/json",
        "md" => "text/markdown",
        "mp3" => "audio/mpeg",
        "mp4" => "video/mp4",
        "mpeg" => "video/mpeg",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "tar" => "application/x-tar",
        "txt" => "text/plain",
        "wav" => "audio/wav",
        "webm" => "video/webm",
        "webp" => "image/webp",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "xml" => "application/xml",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

/// One resolved `--file` entry (run.ts:51-57 `FilePart`).
fn file_part(url: String, filename: String, mime: String) -> Value {
    json!({
        "type": "file",
        "url": url,
        "filename": filename,
        "mime": mime,
    })
}

/// run.ts:357-414 — resolve every `--file` against `base`. Local mode emits
/// `file://` URLs (directories allowed as `application/x-directory`); attach
/// mode inlines content as `data:` URIs (directories and >10 MiB files
/// rejected).
pub fn resolve_files(attach: bool, base: &Path, paths: &[String]) -> Result<Vec<Value>, CliError> {
    let mut files = Vec::new();
    for file_path in paths {
        let resolved = resolve(base, file_path);
        let stat = std::fs::metadata(&resolved);
        if stat.is_err() {
            return Err(CliError::new(format!("File not found: {file_path}")));
        }
        let stat = stat.expect("metadata check above");
        let is_directory = stat.is_dir();
        if attach && is_directory {
            return Err(CliError::new(format!(
                "Cannot attach local directory without a shared filesystem: {file_path}"
            )));
        }
        let filename = resolved
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !attach {
            let mime = if is_directory {
                "application/x-directory"
            } else {
                "text/plain"
            };
            files.push(file_part(
                path_to_file_url(&resolved),
                filename,
                mime.to_string(),
            ));
            continue;
        }
        if !stat.is_file() || stat.len() > ATTACH_FILE_MAX_BYTES {
            return Err(CliError::new(format!(
                "Cannot attach local file larger than 10 MiB or a special file: {file_path}"
            )));
        }
        let content = std::fs::read(&resolved)
            .map_err(|_| CliError::new(format!("File not found: {file_path}")))?;
        // `Buffer.from(text, "utf8").equals(content)` — the content
        // round-trips as UTF-8 exactly.
        let utf8 = String::from_utf8_lossy(&content).into_owned();
        let mime = if utf8.as_bytes() == content.as_slice() {
            "text/plain"
        } else {
            mime_type(&resolved)
        };
        files.push(file_part(
            format!(
                "data:{mime};base64,{}",
                crate::client::base64_encode(&content)
            ),
            filename,
            mime.to_string(),
        ));
    }
    Ok(files)
}

/// `path.resolve(base, p)` — absolute wins, otherwise joined.
fn resolve(base: &Path, p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().to_path_buf();
        (dir, path)
    }

    #[test]
    fn path_to_file_url_keeps_safe_characters() {
        assert_eq!(
            path_to_file_url(Path::new("/a b/c.txt")),
            "file:///a%20b/c.txt"
        );
        assert_eq!(path_to_file_url(Path::new("/x/y.md")), "file:///x/y.md");
    }

    #[test]
    fn local_file_uses_file_url_and_text_plain() {
        let (_guard, dir) = temp_dir();
        std::fs::write(dir.join("note.txt"), "hello").expect("write");
        let files = resolve_files(false, &dir, &["note.txt".to_string()]).unwrap();
        assert_eq!(
            files[0],
            json!({
                "type": "file",
                "url": path_to_file_url(&dir.join("note.txt")),
                "filename": "note.txt",
                "mime": "text/plain",
            })
        );
    }

    #[test]
    fn local_directory_uses_directory_mime() {
        let (_guard, dir) = temp_dir();
        let files = resolve_files(false, &dir, &[".".to_string()]).unwrap();
        assert_eq!(files[0]["mime"], "application/x-directory");
    }

    #[test]
    fn missing_file_is_an_error() {
        let (_guard, dir) = temp_dir();
        let err = resolve_files(false, &dir, &["nope.txt".to_string()]).unwrap_err();
        assert_eq!(err.message, "File not found: nope.txt");
        assert_eq!(err.exit_code, 1);
    }

    #[test]
    fn attach_directory_is_rejected() {
        let (_guard, dir) = temp_dir();
        std::fs::create_dir(dir.join("sub")).expect("mkdir");
        let err = resolve_files(true, &dir, &["sub".to_string()]).unwrap_err();
        assert_eq!(
            err.message,
            "Cannot attach local directory without a shared filesystem: sub"
        );
    }

    #[test]
    fn attach_inlines_utf8_content_as_data_uri() {
        let (_guard, dir) = temp_dir();
        std::fs::write(dir.join("note.txt"), "hello").expect("write");
        let files = resolve_files(true, &dir, &["note.txt".to_string()]).unwrap();
        assert_eq!(files[0]["mime"], "text/plain");
        assert_eq!(
            files[0]["url"],
            format!(
                "data:text/plain;base64,{}",
                crate::client::base64_encode(b"hello")
            )
        );
    }

    #[test]
    fn attach_binary_content_uses_detected_mime() {
        let (_guard, dir) = temp_dir();
        let png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A];
        std::fs::write(dir.join("img.png"), png).expect("write");
        let files = resolve_files(true, &dir, &["img.png".to_string()]).unwrap();
        assert_eq!(files[0]["mime"], "image/png");
    }

    #[test]
    fn attach_oversize_file_is_rejected() {
        let (_guard, dir) = temp_dir();
        let big = dir.join("big.bin");
        std::fs::write(&big, vec![0u8; 0]).expect("write");
        let mut file = std::fs::File::options()
            .write(true)
            .open(&big)
            .expect("open");
        use std::io::Write;
        let chunk = vec![0u8; 1024 * 1024];
        for _ in 0..11 {
            file.write_all(&chunk).expect("grow");
        }
        let err = resolve_files(true, &dir, &["big.bin".to_string()]).unwrap_err();
        assert_eq!(
            err.message,
            "Cannot attach local file larger than 10 MiB or a special file: big.bin"
        );
    }

    #[test]
    fn resolve_prefers_absolute_paths() {
        let (_guard, dir) = temp_dir();
        // A platform-absolute path passes through unchanged; build it from
        // the temp dir so the test holds on every OS.
        let abs = dir.join("abs");
        assert_eq!(resolve(&dir, abs.to_str().unwrap()), abs);
        assert_eq!(resolve(&dir, "a"), dir.join("a"));
    }
}

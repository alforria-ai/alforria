//! `read` tool — port of `tool/read.ts` (spec M4.2).

use std::path::Path;
use std::sync::Arc;

use base64::Engine as _;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::tool::def::{
    define, Agents, AskRequest, Attachment, BoxFuture, ExecuteResult, ToolCtxRef, ToolDef,
};
use crate::tool::error::ToolError;
use crate::tool::external_directory::{assert_external_directory, ExternalOptions, Kind};
use crate::tool::ripgrep::{
    truncate_utf16, ts_basename, ts_dirname, ts_relative, ts_resolve, utf16_len,
};
use crate::tool::truncate::Truncate;

pub const DEFAULT_READ_LIMIT: usize = 2000;
pub const MAX_LINE_LENGTH: usize = 2000;
pub const MAX_LINE_SUFFIX: &str = "... (line truncated to 2000 chars)";
pub const MAX_BYTES: usize = 50 * 1024;
pub const MAX_BYTES_LABEL: &str = "50 KB";
pub const SAMPLE_BYTES: usize = 4096;

const SUPPORTED_IMAGE_MIMES: [&str; 4] = ["image/jpeg", "image/png", "image/gif", "image/webp"];

/// `LSP.touchFile` warm-up seam (read.ts:117-120). Fire-and-forget; the full
/// `Lsp` service seam is M4.8/M7.
pub trait ReadLsp: Send + Sync {
    fn touch_file<'a>(&'a self, filepath: &'a str) -> BoxFuture<'a, ()>;
}

#[derive(Debug, Deserialize)]
pub struct ReadParameters {
    #[serde(rename = "filePath")]
    pub file_path: String,
    pub offset: Option<u64>,
    pub limit: Option<u64>,
}

/// Hand-authored JSON Schema (byte-matches the Effect-generated schema; see
/// `tests/golden/toolschema/read.json`).
pub fn parameters() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {
            "filePath": {
                "type": "string",
                "description": "The absolute path to the file or directory to read"
            },
            "offset": {
                "minimum": 0,
                "type": "integer",
                "maximum": 9007199254740991i64,
                "description": "The line number to start reading from (1-indexed)"
            },
            "limit": {
                "minimum": 0,
                "type": "integer",
                "maximum": 9007199254740991i64,
                "description": "The maximum number of lines to read (defaults to 2000)"
            }
        },
        "required": ["filePath"]
    })
}

/// Build the `read` tool. `lsp` is the fire-and-forget LSP warm-up seam
/// (`None` = no LSP in M4).
pub fn read_tool(
    truncate: Arc<dyn Truncate>,
    agents: Arc<dyn Agents>,
    lsp: Option<Arc<dyn ReadLsp>>,
) -> ToolDef {
    define(
        "read",
        include_str!("txt/read.txt"),
        parameters(),
        None,
        truncate,
        agents,
        move |params: ReadParameters, ctx: ToolCtxRef<'_>| run(params, ctx, lsp.clone()),
    )
}

fn run(
    params: ReadParameters,
    ctx: ToolCtxRef<'_>,
    lsp: Option<Arc<dyn ReadLsp>>,
) -> BoxFuture<'_, Result<ExecuteResult, ToolError>> {
    Box::pin(async move {
        let instance = ctx.instance;
        let filepath = ts_resolve(&instance.directory, &params.file_path);
        let title = ts_relative(&instance.worktree, &filepath);

        let stat = tokio::fs::metadata(&filepath).await.ok();
        let kind = match &stat {
            Some(stat) if stat.is_dir() => Kind::Directory,
            _ => Kind::File,
        };
        assert_external_directory(
            &ctx,
            Some(filepath.to_string_lossy().as_ref()),
            ExternalOptions {
                bypass: ctx.extra.bypass_cwd_check,
                kind,
            },
        )
        .await?;

        ctx.ask
            .ask(AskRequest {
                permission: "read".to_string(),
                patterns: vec![ts_relative(&instance.worktree, &filepath)],
                always: vec!["*".to_string()],
                metadata: json!({}),
            })
            .await?;

        let stat = match stat {
            Some(stat) => stat,
            None => return Err(miss(&filepath).await),
        };

        if stat.is_dir() {
            return directory_result(&params, &filepath, &title).await;
        }

        // `Instruction.resolve` seam — returns nothing in M4 (spec §9 S2).
        let loaded: Vec<String> = Vec::new();

        let sample = read_sample(&filepath, stat.len()).await;
        let mime = sniff_attachment_mime(&sample, &mime_type(&filepath));
        let is_image = SUPPORTED_IMAGE_MIMES.contains(&mime.as_str());
        let is_pdf = mime == "application/pdf";

        if is_image || is_pdf {
            let bytes = tokio::fs::read(&filepath)
                .await
                .map_err(|e| ToolError::Failed(e.to_string()))?;
            let msg = if is_pdf {
                "PDF read successfully"
            } else {
                "Image read successfully"
            };
            return Ok(ExecuteResult {
                title,
                output: msg.to_string(),
                metadata: json!({
                    "preview": msg,
                    "truncated": false,
                    "loaded": loaded,
                }),
                attachments: Some(vec![Attachment {
                    kind: "file",
                    mime: mime.clone(),
                    url: format!(
                        "data:{mime};base64,{}",
                        base64::engine::general_purpose::STANDARD.encode(&bytes)
                    ),
                    filename: None,
                }]),
            });
        }

        if is_binary_file(&filepath, &sample) {
            return Err(ToolError::Failed(format!(
                "Cannot read binary file: {}",
                filepath.display()
            )));
        }

        let file = read_lines(
            &filepath,
            params.limit.unwrap_or(DEFAULT_READ_LIMIT as u64),
            params.offset.unwrap_or(0),
        )
        .await?;

        if file.count < file.offset && !(file.count == 0 && file.offset == 1) {
            return Err(ToolError::Failed(format!(
                "Offset {} is out of range for this file ({} lines)",
                file.offset, file.count
            )));
        }

        let last = file.offset + file.raw.len() as u64 - 1;
        let next = last + 1;
        let truncated = file.more || file.cut;

        let mut output = format!(
            "<path>{}</path>\n<type>file</type>\n<content>\n",
            filepath.display()
        );
        output.push_str(
            &file
                .raw
                .iter()
                .enumerate()
                .map(|(i, line)| format!("{}: {}", i as u64 + file.offset, line))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        if file.cut {
            output.push_str(&format!(
                "\n\n(Output capped at {MAX_BYTES_LABEL}. Showing lines {}-{}. Use offset={next} to continue.)",
                file.offset, last
            ));
        } else if file.more {
            output.push_str(&format!(
                "\n\n(Showing lines {}-{} of {}. Use offset={next} to continue.)",
                file.offset, last, file.count
            ));
        } else {
            output.push_str(&format!("\n\n(End of file - total {} lines)", file.count));
        }
        output.push_str("\n</content>");

        warm_lsp(&filepath, &lsp);

        Ok(ExecuteResult {
            title,
            metadata: json!({
                "preview": file.raw.iter().take(20).cloned().collect::<Vec<_>>().join("\n"),
                "truncated": truncated,
                "loaded": loaded,
                "display": {
                    "type": "file",
                    "path": filepath.display().to_string(),
                    "text": file.raw.join("\n"),
                    "lineStart": file.offset,
                    "lineEnd": last,
                    "totalLines": file.count,
                    "truncated": truncated,
                },
            }),
            output,
            attachments: None,
        })
    })
}

/// `ReadStop`-style stream window (read.ts:137-180).
struct Lines {
    raw: Vec<String>,
    count: u64,
    cut: bool,
    more: bool,
    offset: u64,
}

async fn read_lines(filepath: &Path, limit: u64, offset: u64) -> Result<Lines, ToolError> {
    let bytes = tokio::fs::read(filepath)
        .await
        .map_err(|e| ToolError::Failed(e.to_string()))?;
    let text = String::from_utf8_lossy(bytes.as_slice());

    let offset = if offset == 0 { 1 } else { offset };
    let start = offset - 1;
    let mut raw: Vec<String> = Vec::new();
    let mut count = 0u64;
    let mut used_bytes = 0usize;
    let mut cut = false;
    let mut more = false;

    for line in split_lines(&text) {
        count += 1;
        if count - 1 < start {
            continue;
        }
        if raw.len() as u64 >= limit {
            more = true;
            continue;
        }
        let line = if utf16_len(line) > MAX_LINE_LENGTH {
            format!(
                "{}{MAX_LINE_SUFFIX}",
                truncate_utf16(line, MAX_LINE_LENGTH).expect("over the limit")
            )
        } else {
            line.to_string()
        };
        let size = line.len() + usize::from(!raw.is_empty());
        if used_bytes + size <= MAX_BYTES {
            raw.push(line);
            used_bytes += size;
        } else {
            cut = true;
            more = true;
            break;
        }
    }

    Ok(Lines {
        raw,
        count,
        cut,
        more,
        offset,
    })
}

/// Effect `Stream.splitLines`: `\n`, `\r\n` and standalone `\r` terminate
/// lines; a trailing standalone `\r` yields one final empty line.
fn split_lines(text: &str) -> impl Iterator<Item = &str> {
    let bytes = text.as_bytes();
    let mut start = 0usize;
    let mut index = 0usize;
    let ends_with_lone_cr = bytes.last() == Some(&b'\r');
    let mut done = false;
    std::iter::from_fn(move || {
        if done {
            return None;
        }
        loop {
            match bytes.get(index) {
                None => {
                    done = true;
                    if index > start || ends_with_lone_cr {
                        return Some(&text[start..index]);
                    }
                    return None;
                }
                Some(b'\n') => {
                    let line = &text[start..index];
                    index += 1;
                    start = index;
                    return Some(line);
                }
                Some(b'\r') => {
                    let line = &text[start..index];
                    index += if bytes.get(index + 1) == Some(&b'\n') {
                        2
                    } else {
                        1
                    };
                    start = index;
                    return Some(line);
                }
                _ => index += 1,
            }
        }
    })
}

async fn directory_result(
    params: &ReadParameters,
    filepath: &Path,
    title: &str,
) -> Result<ExecuteResult, ToolError> {
    let items = list_directory(filepath).await?;
    let limit = params.limit.unwrap_or(DEFAULT_READ_LIMIT as u64);
    // `params.offset || 1` — a zero or missing offset reads from line 1.
    let offset = match params.offset {
        Some(offset) if offset > 0 => offset,
        _ => 1,
    };
    let start = (offset - 1) as usize;
    let sliced: Vec<String> = items
        .iter()
        .skip(start)
        .take(limit as usize)
        .cloned()
        .collect();
    let truncated = start + sliced.len() < items.len();

    let output = [
        format!("<path>{}</path>", filepath.display()),
        "<type>directory</type>".to_string(),
        "<entries>".to_string(),
        sliced.join("\n"),
        if truncated {
            format!(
                "\n(Showing {} of {} entries. Use 'offset' parameter to read beyond entry {})",
                sliced.len(),
                items.len(),
                offset as usize + sliced.len()
            )
        } else {
            format!("\n({} entries)", items.len())
        },
        "</entries>".to_string(),
    ]
    .join("\n");

    Ok(ExecuteResult {
        title: title.to_string(),
        output,
        metadata: json!({
            "preview": sliced.iter().take(20).cloned().collect::<Vec<_>>().join("\n"),
            "truncated": truncated,
            "loaded": [],
            "display": {
                "type": "directory",
                "path": filepath.display().to_string(),
                "entries": sliced,
                "offset": offset,
                "totalEntries": items.len(),
                "truncated": truncated,
            },
        }),
        attachments: None,
    })
}

async fn list_directory(filepath: &Path) -> Result<Vec<String>, ToolError> {
    let mut entries = Vec::new();
    let mut reader = match tokio::fs::read_dir(filepath).await {
        Ok(reader) => reader,
        Err(e) => return Err(ToolError::Failed(e.to_string())),
    };
    while let Some(entry) = reader
        .next_entry()
        .await
        .map_err(|e| ToolError::Failed(e.to_string()))?
    {
        let name = entry.file_name().to_string_lossy().to_string();
        let file_type = entry
            .file_type()
            .await
            .map_err(|e| ToolError::Failed(e.to_string()))?;
        if file_type.is_dir() {
            entries.push(format!("{name}/"));
            continue;
        }
        if !file_type.is_symlink() {
            entries.push(name);
            continue;
        }
        // Symlink: suffix when it resolves to a directory.
        let target_is_dir = tokio::fs::metadata(entry.path())
            .await
            .map(|stat| stat.is_dir())
            .unwrap_or_default();
        if target_is_dir {
            entries.push(format!("{name}/"));
        } else {
            entries.push(name);
        }
    }
    entries.sort();
    Ok(entries)
}

/// `ReadTool.miss` (read.ts:76-99).
async fn miss(filepath: &Path) -> ToolError {
    let dir = ts_dirname(filepath);
    let base = ts_basename(filepath).to_lowercase();
    let mut items: Vec<String> = Vec::new();
    if let Ok(mut reader) = tokio::fs::read_dir(&dir).await {
        while let Ok(Some(entry)) = reader.next_entry().await {
            let name = entry.file_name().to_string_lossy().to_string();
            let lower = name.to_lowercase();
            if lower.contains(&base) || base.contains(&lower) {
                items.push(dir.join(&name).display().to_string());
                if items.len() >= 3 {
                    break;
                }
            }
        }
    }
    if items.is_empty() {
        ToolError::Failed(format!("File not found: {}", filepath.display()))
    } else {
        ToolError::Failed(format!(
            "File not found: {}\n\nDid you mean one of these?\n{}",
            filepath.display(),
            items.join("\n")
        ))
    }
}

fn warm_lsp(filepath: &Path, lsp: &Option<Arc<dyn ReadLsp>>) {
    if let Some(lsp) = lsp {
        let lsp = Arc::clone(lsp);
        let filepath = filepath.display().to_string();
        tokio::spawn(async move {
            lsp.touch_file(&filepath).await;
        });
    }
}

/// `readSample` (read.ts:122-135): the first `min(sample, size)` bytes.
async fn read_sample(filepath: &Path, file_size: u64) -> Vec<u8> {
    if file_size == 0 {
        return Vec::new();
    }
    let take = SAMPLE_BYTES.min(file_size as usize);
    use tokio::io::AsyncReadExt;
    let mut file = match tokio::fs::File::open(filepath).await {
        Ok(file) => file,
        Err(_) => return Vec::new(),
    };
    let mut sample = vec![0u8; take];
    match file.read(&mut sample).await {
        Ok(n) => sample.truncate(n),
        Err(_) => return Vec::new(),
    }
    sample
}

/// `isBinaryFile` (read.ts:182-227).
fn is_binary_file(filepath: &Path, sample: &[u8]) -> bool {
    let extension = filepath
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if matches!(
        extension.as_str(),
        "zip"
            | "tar"
            | "gz"
            | "exe"
            | "dll"
            | "so"
            | "class"
            | "jar"
            | "war"
            | "7z"
            | "doc"
            | "docx"
            | "xls"
            | "xlsx"
            | "ppt"
            | "pptx"
            | "odt"
            | "ods"
            | "odp"
            | "bin"
            | "dat"
            | "obj"
            | "o"
            | "a"
            | "lib"
            | "wasm"
            | "pyc"
            | "pyo"
    ) {
        return true;
    }

    if sample.is_empty() {
        return false;
    }

    let mut non_printable = 0usize;
    for &byte in sample {
        if byte == 0 {
            return true;
        }
        if byte < 9 || (byte > 13 && byte < 32) {
            non_printable += 1;
        }
    }
    non_printable as f64 / sample.len() as f64 > 0.3
}

/// `FSUtil.mimeType` reduced to the extensions whose mime type is
/// observable through the image/PDF attachment path (`mime-types` lookup
/// with an `application/octet-stream` fallback).
fn mime_type(filepath: &Path) -> String {
    let extension = filepath
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
    .to_string()
}

/// `sniffAttachmentMime` (util/media.ts:15-25).
fn sniff_attachment_mime(sample: &[u8], fallback: &str) -> String {
    let starts_with = |prefix: &[u8]| sample.starts_with(prefix);
    if starts_with(&[0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]) {
        return "image/png".to_string();
    }
    if starts_with(&[0xff, 0xd8, 0xff]) {
        return "image/jpeg".to_string();
    }
    if starts_with(&[0x47, 0x49, 0x46, 0x38]) {
        return "image/gif".to_string();
    }
    if starts_with(&[0x42, 0x4d]) {
        return "image/bmp".to_string();
    }
    if starts_with(&[0x25, 0x50, 0x44, 0x46, 0x2d]) {
        return "application/pdf".to_string();
    }
    if starts_with(&[0x52, 0x49, 0x46, 0x46])
        && sample.len() > 8
        && sample[8..].starts_with(&[0x57, 0x45, 0x42, 0x50])
    {
        return "image/webp".to_string();
    }
    fallback.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::def::Extra;
    use crate::tool::ripgrep::test_support::{ctx, fixed_agents, instance, RecordingAsk};
    use crate::tool::truncate::TruncateService;
    use serde_json::json;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    fn tool(dir: &Path) -> ToolDef {
        read_tool(
            Arc::new(TruncateService::default_limits(dir.join("tool-output"))),
            fixed_agents(),
            None,
        )
    }

    async fn call(
        dir: &Path,
        args: Value,
    ) -> (
        Result<ExecuteResult, ToolError>,
        Vec<crate::tool::def::AskRequest>,
    ) {
        let def = tool(dir);
        let ask = RecordingAsk::new();
        let inst = instance(dir);
        let extra = Extra::default();
        let ctx = ctx(&ask, &inst, &extra);
        let result = (def.execute)(args, ctx).await;
        (result, ask.requests())
    }

    fn write(dir: &Path, name: &str, contents: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn golden_parameters_schema() {
        let golden = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/golden/toolschema/read.json"
        ))
        .unwrap();
        let golden: Value = serde_json::from_str(&golden).unwrap();
        assert_eq!(parameters(), golden);
    }

    #[tokio::test]
    async fn reads_file_with_line_numbers() {
        let temp = crate::storage::test_support::TempDir::new("read-basic");
        let file = write(temp.path(), "main.txt", "one\ntwo\nthree\n");

        let (result, _) = call(temp.path(), json!({ "filePath": file.to_string_lossy() })).await;

        let result = result.unwrap();
        assert_eq!(result.title, "main.txt");
        assert_eq!(
            result.output,
            format!(
                "<path>{}</path>\n<type>file</type>\n<content>\n1: one\n2: two\n3: three\n\n(End of file - total 3 lines)\n</content>",
                file.display()
            )
        );
        assert_eq!(
            result.metadata,
            json!({
                "preview": "one\ntwo\nthree",
                "truncated": false,
                "loaded": [],
                "display": {
                    "type": "file",
                    "path": file.to_string_lossy(),
                    "text": "one\ntwo\nthree",
                    "lineStart": 1,
                    "lineEnd": 3,
                    "totalLines": 3,
                    "truncated": false,
                },
            })
        );
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn offset_and_limit_emit_more_footer() {
        let temp = crate::storage::test_support::TempDir::new("read-offset");
        let contents = (1..=10)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let file = write(temp.path(), "file.txt", &format!("{contents}\n"));

        let (result, _) = call(
            temp.path(),
            json!({ "filePath": file.to_string_lossy(), "offset": 2, "limit": 3 }),
        )
        .await;

        let result = result.unwrap();
        assert_eq!(
            result.output,
            format!(
                "<path>{}</path>\n<type>file</type>\n<content>\n2: line2\n3: line3\n4: line4\n\n(Showing lines 2-4 of 10. Use offset=5 to continue.)\n</content>",
                file.display()
            )
        );
        assert_eq!(result.metadata["truncated"], json!(true));
        let display = &result.metadata["display"];
        assert_eq!(display["lineStart"], json!(2));
        assert_eq!(display["lineEnd"], json!(4));
        assert_eq!(display["totalLines"], json!(10));
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn offset_out_of_range() {
        let temp = crate::storage::test_support::TempDir::new("read-oor");
        let file = write(temp.path(), "file.txt", "a\nb\nc\n");

        let (result, _) = call(
            temp.path(),
            json!({ "filePath": file.to_string_lossy(), "offset": 5 }),
        )
        .await;

        assert_eq!(
            result.unwrap_err().to_string(),
            "Offset 5 is out of range for this file (3 lines)"
        );
        // offset=1 of an empty file is fine.
        std::fs::write(&file, "").unwrap();
        let (result, _) = call(
            temp.path(),
            json!({ "filePath": file.to_string_lossy(), "offset": 1 }),
        )
        .await;
        let result = result.unwrap();
        assert!(result
            .output
            .ends_with("\n\n(End of file - total 0 lines)\n</content>"));
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn long_lines_are_truncated_with_suffix() {
        let temp = crate::storage::test_support::TempDir::new("read-longline");
        let long = "x".repeat(2_500);
        let file = write(temp.path(), "file.txt", &long);

        let (result, _) = call(temp.path(), json!({ "filePath": file.to_string_lossy() })).await;

        let result = result.unwrap();
        let expected = format!("{}{MAX_LINE_SUFFIX}", "x".repeat(2_000));
        assert!(
            result.output.contains(&format!("1: {expected}")),
            "{}",
            result.output
        );
        assert_eq!(result.metadata["display"]["text"], json!(expected));
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn byte_cap_cuts_the_stream() {
        let temp = crate::storage::test_support::TempDir::new("read-cut");
        let line = "a".repeat(2_000);
        let file = write(temp.path(), "file.txt", &format!("{line}\n").repeat(30));

        let (result, _) = call(temp.path(), json!({ "filePath": file.to_string_lossy() })).await;

        // 25 lines of 2000 chars fit in 50 KB; line 26 busts the budget.
        let result = result.unwrap();
        let output = result.output;
        assert!(
            output.contains(&format!(
                "\n\n(Output capped at {MAX_BYTES_LABEL}. Showing lines 1-25. Use offset=26 to continue.)\n</content>"
            )),
            "{output}"
        );
        assert!(!output.contains("(End of file"));
        let display = &result.metadata["display"];
        assert_eq!(display["lineEnd"], json!(25));
        assert_eq!(display["totalLines"], json!(26)); // the cut line counts
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn binary_files_are_rejected() {
        let temp = crate::storage::test_support::TempDir::new("read-binary");
        // by extension
        let file = write(temp.path(), "archive.zip", "not really a zip");
        let (result, _) = call(temp.path(), json!({ "filePath": file.to_string_lossy() })).await;
        assert_eq!(
            result.unwrap_err().to_string(),
            format!("Cannot read binary file: {}", file.display())
        );

        // by NUL byte
        let file = write(temp.path(), "data.txt", "has a \0 nul");
        let (result, _) = call(temp.path(), json!({ "filePath": file.to_string_lossy() })).await;
        assert_eq!(
            result.unwrap_err().to_string(),
            format!("Cannot read binary file: {}", file.display())
        );

        // by non-printable ratio (> 30%)
        let noisy = format!("ok{}", "\u{1}".repeat(10));
        let file = write(temp.path(), "noisy.txt", &noisy);
        let (result, _) = call(temp.path(), json!({ "filePath": file.to_string_lossy() })).await;
        assert!(
            matches!(result.unwrap_err(), ToolError::Failed(m) if m.starts_with("Cannot read binary file"))
        );
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn images_and_pdfs_become_attachments() {
        let temp = crate::storage::test_support::TempDir::new("read-image");
        let png_bytes: Vec<u8> = vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3];
        std::fs::write(temp.path().join("pixel.png"), &png_bytes).unwrap();

        let (result, _) = call(
            temp.path(),
            json!({ "filePath": temp.path().join("pixel.png").to_string_lossy() }),
        )
        .await;

        let result = result.unwrap();
        assert_eq!(result.output, "Image read successfully");
        let attachment = result.attachments.as_ref().unwrap()[0].clone();
        assert_eq!(attachment.mime, "image/png");
        let expected = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&png_bytes)
        );
        assert_eq!(attachment.url, expected);
        assert_eq!(result.metadata["truncated"], json!(false));

        // A .png whose contents don't sniff still resolves via the
        // extension to an image.
        std::fs::write(temp.path().join("fake.png"), "plain text").unwrap();
        let (result, _) = call(
            temp.path(),
            json!({ "filePath": temp.path().join("fake.png").to_string_lossy() }),
        )
        .await;
        assert_eq!(result.unwrap().output, "Image read successfully");

        // PDF by magic
        std::fs::write(temp.path().join("doc.pdf"), "%PDF-1.7 whatever").unwrap();
        let (result, _) = call(
            temp.path(),
            json!({ "filePath": temp.path().join("doc.pdf").to_string_lossy() }),
        )
        .await;
        assert_eq!(result.unwrap().output, "PDF read successfully");
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn directory_listing_is_sorted_and_suffixed() {
        let temp = crate::storage::test_support::TempDir::new("read-dir");
        std::fs::create_dir(temp.path().join("sub")).unwrap();
        std::fs::write(temp.path().join("zeta.txt"), "x").unwrap();
        std::fs::write(temp.path().join("alpha.txt"), "x").unwrap();

        let (result, _) = call(
            temp.path(),
            json!({ "filePath": temp.path().to_string_lossy() }),
        )
        .await;
        let result = result.unwrap();
        assert_eq!(
            result.output,
            format!(
                "<path>{}</path>\n<type>directory</type>\n<entries>\nalpha.txt\nsub/\nzeta.txt\n\n(3 entries)\n</entries>",
                temp.path().display()
            )
        );
        assert_eq!(result.metadata["truncated"], json!(false));
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn directory_offset_and_limit() {
        let temp = crate::storage::test_support::TempDir::new("read-dir-slice");
        for i in 0..5 {
            std::fs::write(temp.path().join(format!("f{i}.txt")), "x").unwrap();
        }

        let (result, _) = call(
            temp.path(),
            json!({ "filePath": temp.path().to_string_lossy(), "offset": 2, "limit": 2 }),
        )
        .await;

        let result = result.unwrap();
        assert!(
            result
                .output
                .contains("f1.txt\nf2.txt\n\n(Showing 2 of 5 entries. Use 'offset' parameter to read beyond entry 4)\n</entries>"),
            "{}",
            result.output
        );
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinks_to_directories_get_the_slash_suffix() {
        let temp = crate::storage::test_support::TempDir::new("read-dirlink");
        let dir = temp.path().to_path_buf();
        std::fs::create_dir(dir.join("realdir")).unwrap();
        std::fs::write(dir.join("plain.txt"), "x").unwrap();
        symlink(dir.join("realdir"), dir.join("dirlink")).unwrap();
        symlink(dir.join("plain.txt"), dir.join("filelink")).unwrap();

        let (result, _) = call(temp.path(), json!({ "filePath": dir.to_string_lossy() })).await;
        let result = result.unwrap();
        assert!(result.output.contains("dirlink/\n"), "{}", result.output);
        assert!(result.output.contains("filelink\n"), "{}", result.output);
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn miss_suggests_similar_entries() {
        let temp = crate::storage::test_support::TempDir::new("read-miss");
        write(temp.path(), "read.txt", "x");

        let (result, _) = call(
            temp.path(),
            json!({ "filePath": temp.path().join("read.tx").to_string_lossy() }),
        )
        .await;
        let error = result.unwrap_err().to_string();
        assert!(
            error.starts_with(&format!(
                "File not found: {}\n\nDid you mean one of these?\n",
                temp.path().join("read.tx").display()
            )),
            "{error}"
        );
        assert!(error.contains(&temp.path().join("read.txt").display().to_string()));

        // No similar entries -> plain message.
        let (result, _) = call(
            temp.path(),
            json!({ "filePath": temp.path().join("zzz.txt").to_string_lossy() }),
        )
        .await;
        assert_eq!(
            result.unwrap_err().to_string(),
            format!("File not found: {}", temp.path().join("zzz.txt").display())
        );
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn ask_happens_before_the_miss_check() {
        let temp = crate::storage::test_support::TempDir::new("read-ask-order");
        let missing = temp.path().join("nope.txt");

        let def = tool(temp.path());
        let ask = RecordingAsk::new();
        let inst = instance(temp.path());
        let extra = Extra::default();
        let ctx = ctx(&ask, &inst, &extra);
        let result = (def.execute)(json!({ "filePath": missing.to_string_lossy() }), ctx).await;

        assert!(result.is_err());
        let requests = ask.requests();
        assert_eq!(requests.len(), 1, "ask precedes the miss check");
        assert_eq!(requests[0].permission, "read");
        assert_eq!(requests[0].patterns, vec!["nope.txt".to_string()]);
        assert_eq!(requests[0].always, vec!["*".to_string()]);
        assert_eq!(requests[0].metadata, json!({}));
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn external_directory_asks_before_read() {
        let temp = crate::storage::test_support::TempDir::new("read-ext");
        let outside = crate::storage::test_support::TempDir::new("read-ext-outside");
        let file = write(outside.path(), "outside.txt", "content\n");

        let def = tool(temp.path());
        let ask = RecordingAsk::new();
        let inst = instance(temp.path());
        let extra = Extra::default();
        let ctx = ctx(&ask, &inst, &extra);
        let result = (def.execute)(json!({ "filePath": file.to_string_lossy() }), ctx).await;

        result.unwrap();
        let requests = ask.requests();
        assert_eq!(requests[0].permission, "external_directory");
        assert_eq!(
            requests[0].patterns,
            vec![format!("{}/*", outside.path().display())]
        );
        assert_eq!(requests[1].permission, "read");
        std::fs::remove_dir_all(temp.path()).ok();
        std::fs::remove_dir_all(outside.path()).ok();
    }

    #[tokio::test]
    async fn invalid_arguments_message_is_byte_exact() {
        let temp = crate::storage::test_support::TempDir::new("read-invalid");
        let (result, _) = call(temp.path(), json!({ "filePath": "x", "offset": -1 })).await;
        assert!(
            matches!(result.unwrap_err(), ToolError::InvalidArguments { tool, .. } if tool == "read")
        );
        std::fs::remove_dir_all(temp.path()).ok();
    }

    #[tokio::test]
    async fn crlf_and_lone_cr_lines() {
        let temp = crate::storage::test_support::TempDir::new("read-crlf");
        let file = write(temp.path(), "crlf.txt", "a\r\nb\rc\nd");

        let (result, _) = call(temp.path(), json!({ "filePath": file.to_string_lossy() })).await;
        let result = result.unwrap();
        assert!(
            result.output.contains("1: a\n2: b\n3: c\n4: d"),
            "{}",
            result.output
        );
        std::fs::remove_dir_all(temp.path()).ok();
    }
}

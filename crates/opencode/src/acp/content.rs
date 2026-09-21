//! ContentBlock <-> v1 session-parts translation (`acp/content.ts`).

use serde_json::{json, Value};

/// The replay-side part union (`ReplayPart`, content.ts:8-24).
#[derive(Debug, Clone, PartialEq)]
pub enum ReplayPart {
    Text {
        text: String,
        synthetic: bool,
        ignored: bool,
    },
    File {
        url: String,
        mime: String,
        filename: Option<String>,
    },
    Reasoning {
        text: String,
    },
}

/// `promptContentToParts` (content.ts:26-117).
pub fn prompt_content_to_parts(content: &[Value]) -> Vec<Value> {
    content.iter().flat_map(content_block_to_parts).collect()
}

pub fn content_block_to_parts(block: &Value) -> Vec<Value> {
    match block.get("type").and_then(Value::as_str).unwrap_or("") {
        "text" => {
            let text = block.get("text").and_then(Value::as_str).unwrap_or("");
            let mut part = json!({ "type": "text", "text": text });
            for (key, value) in
                audience_flags(block.get("annotations").and_then(|a| a.get("audience")))
                    .as_object()
                    .into_iter()
                    .flatten()
            {
                part[key] = value.clone();
            }
            vec![part]
        }
        "image" => {
            let data = block.get("data").and_then(Value::as_str);
            let mime_type = block
                .get("mimeType")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let uri = block.get("uri").and_then(Value::as_str);
            if let Some(data) = data {
                return vec![json!({
                    "type": "file",
                    "url": format!("data:{mime_type};base64,{data}"),
                    "filename": filename_from_uri(uri).unwrap_or_else(|| "image".to_string()),
                    "mime": mime_type,
                })];
            }
            if let Some(uri) = uri {
                if uri.starts_with("data:") {
                    return vec![json!({
                        "type": "file",
                        "url": uri,
                        "filename": filename_from_uri(Some(uri)).unwrap_or_else(|| "image".to_string()),
                        "mime": mime_type,
                    })];
                }
                if uri.starts_with("http://") || uri.starts_with("https://") {
                    return vec![json!({
                        "type": "file",
                        "url": uri,
                        "filename": filename_from_uri(Some(uri)).unwrap_or_else(|| "image".to_string()),
                        "mime": mime_type,
                    })];
                }
            }
            Vec::new()
        }
        "resource_link" => {
            let link = uri_to_file_part(
                block.get("uri").and_then(Value::as_str).unwrap_or_default(),
                block
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .or(Some("text/plain"))
                    .unwrap_or("text/plain"),
                block.get("name").and_then(Value::as_str),
            );
            if link.get("type").and_then(Value::as_str) == Some("file") {
                vec![link]
            } else {
                vec![
                    json!({ "type": "text", "text": link.get("text").cloned().unwrap_or(Value::Null) }),
                ]
            }
        }
        "resource" => {
            let resource = match block.get("resource") {
                Some(resource) => resource,
                None => return Vec::new(),
            };
            if resource.get("text").is_some() {
                let text = resource
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let uri = resource
                    .get("uri")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match parse_url(uri) {
                    Some(url) if url.scheme == "file" => {
                        let line = url
                            .fragment
                            .as_deref()
                            .and_then(|hash| hash.strip_prefix('L'))
                            .and_then(|rest| rest.split('-').next())
                            .filter(|rest| rest.chars().all(|c| c.is_ascii_digit()));
                        let filepath = file_url_to_path(&url);
                        return vec![json!({
                            "type": "text",
                            "text": format!("[{}{}]\n{}", filepath, line.map(|l| format!(":{l}")).unwrap_or_default(), text),
                        })];
                    }
                    _ => {
                        return vec![json!({
                            "type": "text",
                            "text": format!("[{uri}]\n{text}"),
                        })];
                    }
                }
            }
            if let Some(mime_type) = resource.get("mimeType").and_then(Value::as_str) {
                let uri = resource
                    .get("uri")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let url = if uri.starts_with("data:") {
                    uri.to_string()
                } else {
                    let blob = resource
                        .get("blob")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    format!("data:{mime_type};base64,{blob}")
                };
                return vec![json!({
                    "type": "file",
                    "url": url,
                    "filename": filename_from_uri(Some(uri)).unwrap_or_else(|| "file".to_string()),
                    "mime": mime_type,
                })];
            }
            Vec::new()
        }
        _ => Vec::new(),
    }
}

/// `partsToContentChunks` (content.ts:119-151).
pub fn parts_to_content_chunks(parts: &[ReplayPart]) -> Vec<Value> {
    parts.iter().flat_map(part_to_content_chunks).collect()
}

fn part_to_content_chunks(part: &ReplayPart) -> Vec<Value> {
    match part {
        ReplayPart::Text {
            text,
            synthetic,
            ignored,
        } => {
            if text.is_empty() {
                return Vec::new();
            }
            let mut content = json!({ "type": "text", "text": text });
            if let Some(audience) = part_audience(*synthetic, *ignored) {
                content["annotations"] = json!({ "audience": audience });
            }
            vec![json!({ "content": content })]
        }
        ReplayPart::File {
            url,
            mime,
            filename,
        } => file_part_to_content_chunks(url, mime, filename),
        ReplayPart::Reasoning { text } => {
            if text.is_empty() {
                return Vec::new();
            }
            vec![json!({
                "content": { "type": "text", "text": text },
            })]
        }
    }
}

/// `filePartToContentChunks` (content.ts:190-239).
fn file_part_to_content_chunks(url: &str, mime: &str, filename: &Option<String>) -> Vec<Value> {
    if url.starts_with("file://") {
        return vec![json!({
            "content": {
                "type": "resource_link",
                "uri": url,
                "name": filename.clone().unwrap_or_else(|| "file".to_string()),
                "mimeType": mime,
            },
        })];
    }
    let Some((data_mime, base64)) = decode_data_url(url) else {
        return Vec::new();
    };
    if data_mime.starts_with("image/") {
        return vec![json!({
            "content": {
                "type": "image",
                "mimeType": data_mime,
                "data": base64,
                "uri": format!("file://{}", filename.clone().unwrap_or_else(|| "image".to_string())),
            },
        })];
    }
    let name = filename.clone().unwrap_or_else(|| "file".to_string());
    let resource = if data_mime.starts_with("text/") || data_mime == "application/json" {
        json!({
            "uri": format!("file://{name}"),
            "mimeType": data_mime,
            "text": String::from_utf8_lossy(&decode_base64(&base64)).into_owned(),
        })
    } else {
        json!({
            "uri": format!("file://{name}"),
            "mimeType": data_mime,
            "blob": base64,
        })
    };
    vec![json!({
        "content": {
            "type": "resource",
            "resource": resource,
        },
    })]
}

/// `uriToFilePart` (content.ts:159-188).
fn uri_to_file_part(uri: &str, mime: &str, filename: Option<&str>) -> Value {
    if uri.starts_with("file://") {
        return json!({
            "type": "file",
            "url": uri,
            "filename": filename
                .map(str::to_string)
                .or_else(|| filename_from_uri(Some(uri)))
                .unwrap_or_else(|| "file".to_string()),
            "mime": mime,
        });
    }
    if uri.starts_with("zed://") {
        if let Some(path) = parse_url(uri)
            .and_then(|url| {
                url.query
                    .split('&')
                    .filter_map(|pair| pair.split_once('='))
                    .find(|(key, _)| *key == "path")
                    .map(|(_, value)| percent_decode(value))
            })
            .filter(|path| !path.is_empty())
        {
            return json!({
                "type": "file",
                "url": path_to_file_url(&path),
                "filename": filename
                    .map(str::to_string)
                    .or_else(|| std::path::Path::new(&path)
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned()))
                    .unwrap_or_else(|| "file".to_string()),
                "mime": mime,
            });
        }
    }
    json!({ "type": "text", "text": uri })
}

/// A minimal URL splitter — scheme, path, query, fragment — for the
/// `file://`/`zed://` handling below.
struct ParsedUrl {
    scheme: String,
    path: String,
    query: String,
    fragment: Option<String>,
}

fn parse_url(uri: &str) -> Option<ParsedUrl> {
    let (scheme, rest) = uri.split_once("://")?;
    if scheme.is_empty() {
        return None;
    }
    match rest.split_once('#') {
        Some((rest, fragment)) => Some(split_query(scheme, rest, Some(fragment.to_string()))),
        None => Some(split_query(scheme, rest, None)),
    }
}

fn split_query(scheme: &str, rest: &str, fragment: Option<String>) -> ParsedUrl {
    match rest.split_once('?') {
        Some((path, query)) => ParsedUrl {
            scheme: scheme.to_string(),
            path: path.to_string(),
            query: query.to_string(),
            fragment,
        },
        None => ParsedUrl {
            scheme: scheme.to_string(),
            path: rest.to_string(),
            query: String::new(),
            fragment,
        },
    }
}

/// `fileURLToPath` — strips `file://` without percent-encoding loss.
fn file_url_to_path(url: &ParsedUrl) -> String {
    percent_decode(&url.path)
}

pub fn path_to_file_url(path: &str) -> String {
    format!("file://{path}")
}

fn percent_decode(input: &str) -> String {
    let mut out = String::new();
    let bytes = input.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = &input[index + 1..index + 3];
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte as char);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index] as char);
        index += 1;
    }
    out
}

fn decode_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (mime, base64) = rest.split_once(";base64,")?;
    if base64.is_empty() {
        return None;
    }
    Some((mime.to_string(), base64.to_string()))
}

fn decode_base64(base64: &str) -> Vec<u8> {
    base64.decode()
}

trait Base64Ext {
    fn decode(&self) -> Vec<u8>;
}

impl Base64Ext for &str {
    fn decode(&self) -> Vec<u8> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(self)
            .unwrap_or_default()
    }
}

fn audience_flags(audience: Option<&Value>) -> Value {
    let audience = match audience {
        Some(audience) => audience.as_array(),
        None => None,
    };
    if let Some(audience) = audience {
        if audience.len() == 1 && audience[0] == json!("assistant") {
            return json!({ "synthetic": true });
        }
        if audience.len() == 1 && audience[0] == json!("user") {
            return json!({ "ignored": true });
        }
    }
    json!({})
}

fn part_audience(synthetic: bool, ignored: bool) -> Option<Value> {
    if synthetic {
        Some(json!(["assistant"]))
    } else if ignored {
        Some(json!(["user"]))
    } else {
        None
    }
}

fn filename_from_uri(uri: Option<&str>) -> Option<String> {
    let uri = uri?;
    if uri.starts_with("data:") {
        return None;
    }
    let path = match parse_url(uri) {
        Some(url) => url.path,
        None => uri.to_string(),
    };
    let path = path.split('?').next().unwrap_or(&path);
    let path = percent_decode(path);
    let name = std::path::Path::new(&path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty())?;
    Some(name)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn text_block_becomes_text_part() {
        let parts = prompt_content_to_parts(&[json!({ "type": "text", "text": "hi" })]);
        assert_eq!(parts, vec![json!({ "type": "text", "text": "hi" })]);
    }

    #[test]
    fn assistant_audience_marks_synthetic() {
        let parts = prompt_content_to_parts(&[json!({
            "type": "text",
            "text": "hidden",
            "annotations": { "audience": ["assistant"] },
        })]);
        assert_eq!(
            parts[0]["synthetic"],
            json!(true),
            "audience: [assistant] => synthetic"
        );
    }

    #[test]
    fn image_data_block_becomes_file_part() {
        let parts = prompt_content_to_parts(&[json!({
            "type": "image",
            "mimeType": "image/png",
            "data": "AAAA",
            "uri": "file:///tmp/shot.png",
        })]);
        assert_eq!(parts[0]["type"], json!("file"));
        assert_eq!(parts[0]["url"], json!("data:image/png;base64,AAAA"));
        assert_eq!(parts[0]["filename"], json!("shot.png"));
        assert_eq!(parts[0]["mime"], json!("image/png"));
    }

    #[test]
    fn http_image_uri_becomes_file_part() {
        let parts = prompt_content_to_parts(&[json!({
            "type": "image",
            "mimeType": "image/png",
            "uri": "https://example.com/pic.png",
        })]);
        assert_eq!(parts[0]["url"], json!("https://example.com/pic.png"));
        assert_eq!(parts[0]["filename"], json!("pic.png"));
    }

    #[test]
    fn file_resource_link_becomes_file_part() {
        let parts = prompt_content_to_parts(&[json!({
            "type": "resource_link",
            "uri": "file:///tmp/a.txt",
            "mimeType": "text/plain",
            "name": "a.txt",
        })]);
        assert_eq!(parts[0]["type"], json!("file"));
        assert_eq!(parts[0]["url"], json!("file:///tmp/a.txt"));
    }

    #[test]
    fn text_resource_in_file_uri_renders_path_reference() {
        let parts = prompt_content_to_parts(&[json!({
            "type": "resource",
            "resource": {
                "uri": "file:///tmp/notes.md#L42",
                "mimeType": "text/markdown",
                "text": "content",
            },
        })]);
        assert_eq!(
            parts[0]["text"],
            json!("[/tmp/notes.md:42]\ncontent"),
            "file: uri renders [path:line] + text"
        );
    }

    #[test]
    fn blob_resource_becomes_file_part() {
        let parts = prompt_content_to_parts(&[json!({
            "type": "resource",
            "resource": {
                "uri": "file:///tmp/x.bin",
                "mimeType": "application/octet-stream",
                "blob": "AAAA",
            },
        })]);
        assert_eq!(parts[0]["type"], json!("file"));
        assert_eq!(
            parts[0]["url"],
            json!("data:application/octet-stream;base64,AAAA")
        );
    }

    #[test]
    fn text_part_replays_with_annotations() {
        let chunks = parts_to_content_chunks(&[ReplayPart::Text {
            text: "hi".to_string(),
            synthetic: true,
            ignored: false,
        }]);
        assert_eq!(
            chunks[0]["content"]["annotations"]["audience"],
            json!(["assistant"])
        );
    }

    #[test]
    fn empty_text_replays_nothing() {
        assert!(parts_to_content_chunks(&[ReplayPart::Text {
            text: String::new(),
            synthetic: false,
            ignored: false,
        }])
        .is_empty());
    }

    #[test]
    fn data_url_image_replays_as_image_chunk() {
        let chunks = parts_to_content_chunks(&[ReplayPart::File {
            url: "data:image/png;base64,AAAA".to_string(),
            mime: "image/png".to_string(),
            filename: Some("img".to_string()),
        }]);
        assert_eq!(chunks[0]["content"]["type"], json!("image"));
        assert_eq!(chunks[0]["content"]["data"], json!("AAAA"));
    }

    #[test]
    fn file_url_replays_as_resource_link() {
        let chunks = parts_to_content_chunks(&[ReplayPart::File {
            url: "file:///tmp/a.txt".to_string(),
            mime: "text/plain".to_string(),
            filename: Some("a.txt".to_string()),
        }]);
        assert_eq!(chunks[0]["content"]["type"], json!("resource_link"));
    }
}

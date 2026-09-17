//! code-mode `execute` tool — port of `tool/code-mode.ts` (spec M4.8).
//!
//! The confined JS interpreter (`@opencode-ai/codemode`) is **not portable
//! in M4**: the engine choice (embedded JS runtime vs WASM) is an escalated
//! follow-up (spec STOP S8). The pure projection helpers and the catalog
//! grouping are ported and tested here.

use serde_json::Value;

pub const CODE_MODE_TOOL: &str = "execute";
pub const DESCRIPTION: &str =
    "Run a confined orchestration script with access to connected MCP tools.";

/// One entry of the grouped catalog (code-mode.ts:46-53).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogEntry {
    pub path: String,
    pub key: String,
    pub server: String,
    pub local: String,
}

/// `groupByServer` (code-mode.ts:55-73) — `Record<key, McpTool>` grouped by
/// longest matching server prefix (`server_local` keys).
pub fn group_by_server(
    mcp_tools: Vec<String>,
    servers: &[String],
) -> Vec<(String, Vec<CatalogEntry>)> {
    let mut by_longest: Vec<&String> = servers.iter().collect();
    by_longest.sort_by_key(|server| std::cmp::Reverse(server.len()));

    let mut keys = mcp_tools;
    keys.sort();

    let mut groups: Vec<(String, Vec<CatalogEntry>)> = Vec::new();
    for key in keys {
        let server = by_longest
            .iter()
            .find(|name| key.starts_with(&format!("{name}_")))
            .cloned()
            .map(|name| name.to_string())
            .unwrap_or_else(|| {
                if key.contains('_') {
                    key[..key.find('_').unwrap()].to_string()
                } else {
                    key.clone()
                }
            });
        let local = if key.starts_with(&format!("{server}_")) {
            key[server.len() + 1..].to_string()
        } else {
            key.clone()
        };
        let entry = CatalogEntry {
            path: format!("{server}.{local}"),
            key: key.clone(),
            server: server.clone(),
            local,
        };
        match groups.iter_mut().find(|(name, _)| name == &server) {
            Some((_, entries)) => entries.push(entry),
            None => groups.push((server, vec![entry])),
        }
    }
    groups
}

/// One content block of an MCP tool result.
#[derive(Debug, Clone)]
pub enum McpBlock {
    Text {
        text: String,
    },
    Image {
        mime_type: String,
        data: String,
    },
    Audio {
        mime_type: String,
        data: String,
    },
    ResourceText {
        text: String,
    },
    ResourceBlob {
        mime_type: Option<String>,
        blob: String,
        uri: String,
    },
    ResourceLink {
        name: String,
        uri: String,
    },
}

/// An attachment collected while projecting a result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectedAttachment {
    pub kind: &'static str,
    pub mime: String,
    pub url: String,
    pub filename: Option<String>,
}

/// `lastSegment` (code-mode.ts:95-99).
fn last_segment(uri: &str) -> Option<String> {
    let trimmed = uri
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .trim_end_matches('/');
    let segment = &trimmed[trimmed.rfind('/').map(|i| i + 1).unwrap_or(0)..];
    if segment.is_empty() {
        None
    } else {
        Some(segment.to_string())
    }
}

fn data_url(mime: &str, base64: &str) -> String {
    format!("data:{mime};base64,{base64}")
}

/// `projectMcpResult` (code-mode.ts:101-133): project an MCP result into a
/// text/structured value plus attachments.
pub fn project_mcp_result(
    blocks: &[McpBlock],
    structured: Option<Value>,
) -> (Option<Value>, Vec<ProjectedAttachment>) {
    let mut text: Vec<String> = Vec::new();
    let mut files = 0;
    let mut images = 0;
    let mut attachments: Vec<ProjectedAttachment> = Vec::new();

    let mut push = |attachment: ProjectedAttachment, attachments: &mut Vec<ProjectedAttachment>| {
        files += 1;
        if attachment.mime.starts_with("image/") {
            images += 1;
        }
        attachments.push(attachment);
    };

    for block in blocks {
        match block {
            McpBlock::Text { text: value } => text.push(value.clone()),
            McpBlock::Image { mime_type, data } => push(
                ProjectedAttachment {
                    kind: "file",
                    mime: mime_type.clone(),
                    url: data_url(mime_type, data),
                    filename: None,
                },
                &mut attachments,
            ),
            McpBlock::Audio { mime_type, data } => push(
                ProjectedAttachment {
                    kind: "file",
                    mime: mime_type.clone(),
                    url: data_url(mime_type, data),
                    filename: None,
                },
                &mut attachments,
            ),
            McpBlock::ResourceText { text: value } => text.push(value.clone()),
            McpBlock::ResourceBlob {
                mime_type,
                blob,
                uri,
            } => {
                let mime = mime_type
                    .clone()
                    .unwrap_or_else(|| "application/octet-stream".to_string());
                push(
                    ProjectedAttachment {
                        kind: "file",
                        mime: mime.clone(),
                        url: data_url(&mime, blob),
                        filename: last_segment(uri),
                    },
                    &mut attachments,
                );
            }
            McpBlock::ResourceLink { name, uri } => {
                // A link is a reference, not fetchable media.
                text.push(format!("{name}: {uri}"));
            }
        }
    }

    if let Some(structured) = structured {
        if !structured.is_null() {
            return (Some(structured), attachments);
        }
    }
    if !text.is_empty() {
        return (Some(Value::String(text.join("\n"))), attachments);
    }
    if files > 0 {
        let noun = if files == images { "image" } else { "file" };
        return (
            Some(Value::String(format!(
                "[{files} {noun}{} attached to the result]",
                if files == 1 { "" } else { "s" }
            ))),
            attachments,
        );
    }
    (None, attachments)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn groups_by_longest_server_prefix() {
        let groups = group_by_server(
            vec![
                "a_tool1".to_string(),
                "ab_tool2".to_string(),
                "b_tool3".to_string(),
            ],
            &["a".to_string(), "ab".to_string()],
        );
        assert_eq!(groups.len(), 3);
        let (a, entries) = &groups[0];
        assert_eq!(a, "a");
        assert_eq!(entries.len(), 1);
        let (ab, entries) = &groups[1];
        assert_eq!(ab, "ab");
        assert_eq!(entries[0].local, "tool2");
        assert_eq!(entries[0].path, "ab.tool2");
        let (b, entries) = &groups[2];
        assert_eq!(b, "b");
        assert_eq!(entries[0].path, "b.tool3");
    }

    #[test]
    fn serverless_key_uses_prefix() {
        let groups = group_by_server(vec!["nounderscore".to_string()], &[]);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].1[0].server, "nounderscore");
    }

    #[test]
    fn project_text_blocks_join() {
        let (value, attachments) = project_mcp_result(
            &[
                McpBlock::Text {
                    text: "one".to_string(),
                },
                McpBlock::Text {
                    text: "two".to_string(),
                },
            ],
            None,
        );
        assert_eq!(value, Some(json!("one\ntwo")));
        assert!(attachments.is_empty());
    }

    #[test]
    fn project_structured_passthrough() {
        let (value, _) = project_mcp_result(
            &[McpBlock::Text {
                text: "ignored".to_string(),
            }],
            Some(json!({ "kind": "ok" })),
        );
        assert_eq!(value, Some(json!({ "kind": "ok" })));
    }

    #[test]
    fn project_image_attachments() {
        let (_, attachments) = project_mcp_result(
            &[McpBlock::Image {
                mime_type: "image/png".to_string(),
                data: "AAAA".to_string(),
            }],
            None,
        );
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].mime, "image/png");
        assert_eq!(attachments[0].url, "data:image/png;base64,AAAA");

        let (value, _) = project_mcp_result(
            &[McpBlock::Image {
                mime_type: "image/png".to_string(),
                data: "AAAA".to_string(),
            }],
            None,
        );
        assert_eq!(value, Some(json!("[1 image attached to the result]")));
    }

    #[test]
    fn project_resource_blob_and_link() {
        let (value, attachments) = project_mcp_result(
            &[
                McpBlock::ResourceBlob {
                    mime_type: None,
                    blob: "ZGF0YQ==".to_string(),
                    uri: "file:///tmp/x.tar.gz?raw=1".to_string(),
                },
                McpBlock::ResourceLink {
                    name: "docs".to_string(),
                    uri: "file:///docs".to_string(),
                },
            ],
            None,
        );
        // The link is text, so text wins over the attachment note.
        assert_eq!(value, Some(json!("docs: file:///docs")));
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0].filename, Some("x.tar.gz".to_string()));
        assert_eq!(attachments[0].mime, "application/octet-stream");
    }

    #[test]
    fn project_empty_is_none() {
        let (value, attachments) = project_mcp_result(&[], None);
        assert_eq!(value, None);
        assert!(attachments.is_empty());
    }

    #[test]
    fn multiple_files_pluralized() {
        let (value, _) = project_mcp_result(
            &[
                McpBlock::Image {
                    mime_type: "image/png".to_string(),
                    data: "A".to_string(),
                },
                McpBlock::Image {
                    mime_type: "image/jpeg".to_string(),
                    data: "B".to_string(),
                },
            ],
            None,
        );
        assert_eq!(value, Some(json!("[2 images attached to the result]")));
    }
}

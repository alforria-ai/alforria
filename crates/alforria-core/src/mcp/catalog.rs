//! MCP catalog helpers — port of `mcp/catalog.ts`.

use serde_json::Value;

/// `DEFAULT_TIMEOUT` (catalog.ts:11, index.ts:38).
pub const DEFAULT_TIMEOUT_MS: u64 = 30_000;

/// `MAX_LIST_PAGES` (catalog.ts:12).
const MAX_LIST_PAGES: usize = 1_000;

/// One MCP tool definition (`ToolSchema`, catalog.ts:15-16) — `name`,
/// `description?`, `inputSchema`; kept as raw JSON because consumers adapt
/// it to their own tool format.
#[derive(Debug, Clone, PartialEq)]
pub struct McpToolDef {
    pub name: String,
    pub description: Option<String>,
    pub input_schema: Value,
}

/// `sanitize` (catalog.ts:117).
pub fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// `toolName` (catalog.ts:119).
pub fn tool_name(client_name: &str, name: &str) -> String {
    format!("{}_{}", sanitize(client_name), sanitize(name))
}

/// The `resourceClient` key escaping (catalog.ts:105-106).
pub fn escape_client(client: &str) -> String {
    client.replace('%', "%25").replace(':', "%3A")
}

/// `paginate` (catalog.ts:18-35): follow `nextCursor` until exhausted,
/// rejecting cursors that repeat.
pub async fn paginate<T, F, Fut>(
    mut list: F,
    mut items: impl FnMut(&Value) -> Result<Vec<T>, String>,
) -> Result<Vec<T>, String>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: std::future::Future<Output = Result<Value, String>>,
{
    let mut result: Vec<T> = Vec::new();
    let mut cursors: Vec<String> = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_LIST_PAGES {
        let page = list(cursor.clone()).await?;
        let cursor_key = page
            .get("nextCursor")
            .and_then(Value::as_str)
            .map(String::from);
        result.extend(items(&page)?);
        let Some(next) = cursor_key else {
            return Ok(result);
        };
        if cursors.contains(&next) {
            return Err(format!("MCP list returned duplicate cursor: {next}"));
        }
        cursors.push(next.clone());
        cursor = Some(next);
    }
    Err(format!("MCP list exceeded {MAX_LIST_PAGES} pages"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sanitize_replaces_disallowed_chars() {
        assert_eq!(sanitize("a-b_c"), "a-b_c");
        assert_eq!(sanitize("my server:tool"), "my_server_tool");
    }

    #[test]
    fn tool_name_combines_client_and_tool() {
        assert_eq!(tool_name("my server", "echo tool"), "my_server_echo_tool");
    }

    #[tokio::test]
    async fn paginate_follows_and_stops_cursors() {
        let pages = [
            json!({"items": [1, 2], "nextCursor": "a"}),
            json!({"items": [3], "nextCursor": "b"}),
            json!({"items": [4]}),
        ];
        let pages = &pages;
        let calls = &std::cell::Cell::new(0);
        let collected: Vec<i32> = paginate(
            |cursor| async move {
                let index = match cursor.as_deref() {
                    None => 0,
                    Some("a") => 1,
                    Some("b") => 2,
                    Some(other) => panic!("unexpected cursor {other}"),
                };
                calls.set(calls.get() + 1);
                Ok(pages[index].clone())
            },
            |page| {
                Ok(page["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(|v| v.as_i64().map(|n| n as i32))
                    .collect())
            },
        )
        .await
        .unwrap();
        assert_eq!(collected, vec![1, 2, 3, 4]);
        assert_eq!(calls.get(), 3);
    }

    #[tokio::test]
    async fn paginate_rejects_duplicate_cursor() {
        let err = paginate::<i32, _, _>(
            |_| async { Ok(json!({"items": [1], "nextCursor": "a"})) },
            |_| -> Result<Vec<i32>, String> { Ok(vec![1]) },
        )
        .await
        .unwrap_err();
        assert_eq!(err, "MCP list returned duplicate cursor: a");
    }

    #[test]
    fn server_uri_keys_escape_separator() {
        // `fetch`'s key building (catalog.ts:105-109).
        assert_eq!(escape_client("a:b%server"), "a%3Ab%25server");
    }
}

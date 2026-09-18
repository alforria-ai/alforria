//! MCP OAuth credential store — port of `mcp/auth.ts`.
//!
//! `<Global.Path.data>/mcp-auth.json` (auth.ts:37), written with `0o600`
//! (`auth.ts:80`). The flock-based inter-process lock is reduced to an
//! in-process mutex — the Rust port holds one store per service and the
//! file writes are atomic enough for the single-process CLI/server paths.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `Tokens` (auth.ts:9-14).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct McpTokens {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

/// `ClientInfo` (auth.ts:17-22).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct McpClientInfo {
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id_issued_at: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret_expires_at: Option<f64>,
}

/// `Entry` (auth.ts:25-31).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct McpAuthEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<McpTokens>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_info: Option<McpClientInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_verifier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_url: Option<String>,
}

impl McpAuthEntry {
    fn to_json(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

/// The `McpAuth.Service` interface (auth.ts:40-53).
#[derive(Clone)]
pub struct McpAuth {
    path: PathBuf,
    data: Arc<Mutex<BTreeMap<String, McpAuthEntry>>>,
}

impl McpAuth {
    pub fn new(path: PathBuf) -> McpAuth {
        McpAuth {
            path,
            data: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    fn entries(&self) -> BTreeMap<String, McpAuthEntry> {
        self.data.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// `read` (auth.ts:65-70): missing/invalid files decode as empty.
    fn mutate(&self, update: impl FnOnce(&mut BTreeMap<String, McpAuthEntry>) -> bool) {
        let mut data = self.data.lock().unwrap_or_else(|p| p.into_inner());
        if update(&mut data) {
            let map: BTreeMap<&str, Value> = data
                .iter()
                .map(|(key, entry)| (key.as_str(), entry.to_json()))
                .collect();
            if let Ok(json) = serde_json::to_string_pretty(&map) {
                let _ = write_private(&self.path, &json);
            }
        }
    }

    /// `all` (auth.ts:72-74).
    pub fn all(&self) -> BTreeMap<String, McpAuthEntry> {
        self.entries()
    }

    /// `get` (auth.ts:84-87).
    pub fn get(&self, mcp_name: &str) -> Option<McpAuthEntry> {
        self.entries().get(mcp_name).cloned()
    }

    /// `getForUrl` (auth.ts:89-95): entries only apply to their own URL.
    pub fn get_for_url(&self, mcp_name: &str, server_url: &str) -> Option<McpAuthEntry> {
        let entry = self.get(mcp_name)?;
        if entry.server_url.as_deref() != Some(server_url) {
            return None;
        }
        Some(entry)
    }

    /// `set` (auth.ts:97-102).
    pub fn set(&self, mcp_name: &str, mut entry: McpAuthEntry, server_url: Option<&str>) {
        if let Some(server_url) = server_url {
            entry.server_url = Some(server_url.to_string());
        }
        self.mutate(|data| {
            data.insert(mcp_name.to_string(), entry);
            true
        });
    }

    /// `remove` (auth.ts:104-110).
    pub fn remove(&self, mcp_name: &str) {
        self.mutate(|data| data.remove(mcp_name).is_some());
    }

    /// `updateField` (auth.ts:112-120).
    fn update_entry(&self, mcp_name: &str, update: impl FnOnce(&mut McpAuthEntry)) {
        self.mutate(|data| {
            let mut entry = data.get(mcp_name).cloned().unwrap_or_default();
            update(&mut entry);
            data.insert(mcp_name.to_string(), entry);
            true
        });
    }

    /// `updateTokens` (auth.ts:132).
    pub fn update_tokens(&self, mcp_name: &str, tokens: McpTokens, server_url: Option<&str>) {
        self.update_entry(mcp_name, |entry| {
            entry.tokens = Some(tokens);
            if let Some(server_url) = server_url {
                entry.server_url = Some(server_url.to_string());
            }
        });
    }

    /// `updateClientInfo` (auth.ts:133).
    pub fn update_client_info(
        &self,
        mcp_name: &str,
        client_info: McpClientInfo,
        server_url: Option<&str>,
    ) {
        self.update_entry(mcp_name, |entry| {
            entry.client_info = Some(client_info);
            if let Some(server_url) = server_url {
                entry.server_url = Some(server_url.to_string());
            }
        });
    }

    /// `updateCodeVerifier` (auth.ts:134).
    pub fn update_code_verifier(&self, mcp_name: &str, code_verifier: &str) {
        self.update_entry(mcp_name, |entry| {
            entry.code_verifier = Some(code_verifier.to_string());
        });
    }

    /// `clearCodeVerifier` (auth.ts:122-130, 136).
    pub fn clear_code_verifier(&self, mcp_name: &str) {
        self.update_entry(mcp_name, |entry| {
            entry.code_verifier = None;
        });
    }

    /// `updateOAuthState` (auth.ts:135).
    pub fn update_oauth_state(&self, mcp_name: &str, oauth_state: &str) {
        self.update_entry(mcp_name, |entry| {
            entry.oauth_state = Some(oauth_state.to_string());
        });
    }

    /// `getOAuthState` (auth.ts:139-142).
    pub fn get_oauth_state(&self, mcp_name: &str) -> Option<String> {
        self.get(mcp_name).and_then(|entry| entry.oauth_state)
    }

    /// `clearOAuthState` (auth.ts:122-130, 137).
    pub fn clear_oauth_state(&self, mcp_name: &str) {
        self.update_entry(mcp_name, |entry| {
            entry.oauth_state = None;
        });
    }
}

/// `fs.writeJson(filepath, next, 0o600)` (auth.ts:80) — mode is
/// best-effort (`600` under unix).
fn write_private(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(contents.as_bytes())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (McpAuth, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp-auth.json");
        (McpAuth::new(path.clone()), path)
    }

    #[test]
    fn get_for_url_requires_matching_server_url() {
        let (auth, _) = store();
        auth.set(
            "srv",
            McpAuthEntry {
                tokens: Some(McpTokens {
                    access_token: "at".into(),
                    ..Default::default()
                }),
                server_url: Some("https://mcp.test".into()),
                ..Default::default()
            },
            None,
        );
        assert!(auth.get_for_url("srv", "https://mcp.test").is_some());
        assert!(auth.get_for_url("srv", "https://other.test").is_none());
    }

    #[test]
    fn update_and_clear_fields() {
        let (auth, path) = store();
        auth.update_tokens(
            "srv",
            McpTokens {
                access_token: "at".into(),
                ..Default::default()
            },
            Some("https://mcp.test"),
        );
        auth.update_code_verifier("srv", "ver");
        auth.update_oauth_state("srv", "st");
        assert_eq!(auth.get_oauth_state("srv").as_deref(), Some("st"));
        let entry = auth.get("srv").unwrap();
        assert_eq!(entry.tokens.as_ref().unwrap().access_token, "at");
        assert_eq!(entry.code_verifier.as_deref(), Some("ver"));
        assert!(path.exists());
        auth.clear_code_verifier("srv");
        auth.clear_oauth_state("srv");
        let entry = auth.get("srv").unwrap();
        assert_eq!(entry.code_verifier, None);
        assert_eq!(entry.oauth_state, None);
    }

    #[test]
    fn remove_drops_entry() {
        let (auth, _) = store();
        auth.set("srv", McpAuthEntry::default(), None);
        auth.remove("srv");
        assert!(auth.get("srv").is_none());
    }
}

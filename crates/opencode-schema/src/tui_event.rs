//! Wire DTOs for `schema-src/tui-event.ts` — openapi `TuiPromptAppend`,
//! `TuiCommandExecute`, `TuiToastShow`, `TuiSessionSelect`.

use serde::{Deserialize, Serialize};

use crate::ids::SessionId;

/// `tui.prompt.append` payload — openapi `TuiPromptAppend.data`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TuiPromptAppendData {
    pub text: String,
}

/// `tui.command.execute` payload — openapi `TuiCommandExecute.data`.
///
/// `command` is a free string on the wire (union of known literals and any
/// string — STOP S3): do NOT tighten into a Rust enum.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TuiCommandExecuteData {
    pub command: String,
}

/// `tui.toast.show` payload — openapi `TuiToastShow.data`.
///
/// `duration` uses TS `withDecodingDefault(5000)` — modeled as `Option<u64>`
/// (absent when missing); consumers apply the default (STOP S2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TuiToastShowData {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub message: String,
    pub variant: TuiToastVariant,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<u64>,
}

/// Toast variant — openapi `TuiToastShow.data.variant`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TuiToastVariant {
    Info,
    Success,
    Warning,
    Error,
}

/// `tui.session.select` payload — openapi `TuiSessionSelect.data`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TuiSessionSelectData {
    #[serde(rename = "sessionID")]
    pub session_id: SessionId,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{TuiToastShowData, TuiToastVariant};

    /// openapi `TuiToastShow` vector: `title` and `duration` omitted.
    #[test]
    fn tui_toast_show_omits_title_and_duration() {
        let value = json!({"message": "done", "variant": "success"});
        let data: TuiToastShowData = serde_json::from_value(value.clone()).unwrap();
        let json = serde_json::to_value(&data).unwrap();
        assert_eq!(json, value);
        assert!(json.get("title").is_none());
        assert!(json.get("duration").is_none());
    }

    #[test]
    fn tui_toast_variant_values() {
        assert_eq!(
            serde_json::to_value(TuiToastVariant::Info).unwrap(),
            json!("info")
        );
        assert_eq!(
            serde_json::to_value(TuiToastVariant::Success).unwrap(),
            json!("success")
        );
        assert_eq!(
            serde_json::to_value(TuiToastVariant::Warning).unwrap(),
            json!("warning")
        );
        assert_eq!(
            serde_json::to_value(TuiToastVariant::Error).unwrap(),
            json!("error")
        );
    }
}

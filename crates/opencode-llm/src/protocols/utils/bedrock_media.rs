//! Bedrock Converse media helpers (from `protocols/utils/bedrock-media.ts`).

#![allow(clippy::result_large_err)]

use serde::{Deserialize, Serialize};

use crate::protocols::shared;
use crate::schema::errors::LlmError;
use crate::schema::messages::ContentPart;

// Bedrock Converse accepts image `format` as the file extension and
// `source.bytes` as base64 in the JSON wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    Png,
    Jpeg,
    Gif,
    Webp,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageBlock {
    pub image: ImageBody,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageBody {
    pub format: ImageFormat,
    pub source: MediaSource,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaSource {
    pub bytes: String,
}

// Bedrock document blocks require a user-facing name so the model can refer to
// the uploaded document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DocumentFormat {
    Pdf,
    Csv,
    Doc,
    Docx,
    Xls,
    Xlsx,
    Html,
    Txt,
    Md,
}

impl DocumentFormat {
    fn as_str(self) -> &'static str {
        match self {
            DocumentFormat::Pdf => "pdf",
            DocumentFormat::Csv => "csv",
            DocumentFormat::Doc => "doc",
            DocumentFormat::Docx => "docx",
            DocumentFormat::Xls => "xls",
            DocumentFormat::Xlsx => "xlsx",
            DocumentFormat::Html => "html",
            DocumentFormat::Txt => "txt",
            DocumentFormat::Md => "md",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocumentBlock {
    pub document: DocumentBody,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DocumentBody {
    pub format: DocumentFormat,
    pub name: String,
    pub source: MediaSource,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum BedrockMediaBlock {
    Image(ImageBlock),
    Document(DocumentBlock),
}

const IMAGE_MIMES: [&str; 5] = [
    "image/png",
    "image/jpeg",
    "image/jpg",
    "image/gif",
    "image/webp",
];

const DOCUMENT_MIMES: [&str; 9] = [
    "application/pdf",
    "text/csv",
    "application/msword",
    "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    "application/vnd.ms-excel",
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
    "text/html",
    "text/plain",
    "text/markdown",
];

fn image_format_for(mime: &str) -> Option<ImageFormat> {
    match mime {
        "image/png" => Some(ImageFormat::Png),
        "image/jpeg" | "image/jpg" => Some(ImageFormat::Jpeg),
        "image/gif" => Some(ImageFormat::Gif),
        "image/webp" => Some(ImageFormat::Webp),
        _ => None,
    }
}

fn document_format_for(mime: &str) -> Option<DocumentFormat> {
    match mime {
        "application/pdf" => Some(DocumentFormat::Pdf),
        "text/csv" => Some(DocumentFormat::Csv),
        "application/msword" => Some(DocumentFormat::Doc),
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => {
            Some(DocumentFormat::Docx)
        }
        "application/vnd.ms-excel" => Some(DocumentFormat::Xls),
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet" => {
            Some(DocumentFormat::Xlsx)
        }
        "text/html" => Some(DocumentFormat::Html),
        "text/plain" => Some(DocumentFormat::Txt),
        "text/markdown" => Some(DocumentFormat::Md),
        _ => None,
    }
}

/// Route by MIME. Known image/document formats lower into a typed block;
/// anything else fails with a clear error instead of silently degrading to a
/// malformed document block. Image MIME types not in `IMAGE_MIMES`
/// (e.g. `image/svg+xml`) get an image-specific error so the caller knows it's
/// a format-support issue, not a kind-detection issue.
pub fn lower(part: &ContentPart) -> Result<BedrockMediaBlock, LlmError> {
    let (media_type, filename) = match part {
        ContentPart::Media {
            media_type,
            filename,
            ..
        } => (media_type, filename),
        _ => {
            return Err(LlmError::invalid(
                "Bedrock Converse media lowering requires a media content part",
            ))
        }
    };
    let mime = media_type.to_lowercase();
    if let Some(format) = image_format_for(&mime) {
        let media = shared::validate_media("Bedrock Converse", part, &IMAGE_MIMES)?;
        return Ok(BedrockMediaBlock::Image(ImageBlock {
            image: ImageBody {
                format,
                source: MediaSource {
                    bytes: media.base64,
                },
            },
        }));
    }
    if mime.starts_with("image/") {
        return Err(LlmError::invalid(format!(
            "Bedrock Converse does not support image media type {media_type}"
        )));
    }
    if let Some(format) = document_format_for(&mime) {
        let media = shared::validate_media("Bedrock Converse", part, &DOCUMENT_MIMES)?;
        let name = filename
            .clone()
            .unwrap_or_else(|| format!("document.{}", format.as_str()));
        return Ok(BedrockMediaBlock::Document(DocumentBlock {
            document: DocumentBody {
                format,
                name,
                source: MediaSource {
                    bytes: media.base64,
                },
            },
        }));
    }
    Err(LlmError::invalid(format!(
        "Bedrock Converse does not support media type {media_type}"
    )))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::schema::errors::LlmErrorReason;

    fn reason_message(error: &LlmError) -> &str {
        match &error.reason {
            LlmErrorReason::InvalidRequest { message, .. } => message,
            _ => "",
        }
    }

    #[test]
    fn lower_image_parts_from_a_data_url() {
        let part = ContentPart::media("image/png", "data:image/png;base64,aGVsbG8=");
        let block = lower(&part).unwrap();
        assert_eq!(
            serde_json::to_value(&block).unwrap(),
            json!({"image": {"format": "png", "source": {"bytes": "aGVsbG8="}}}),
        );
    }

    #[test]
    fn lower_image_parts_accept_jpg_alias_and_raw_base64() {
        let part = ContentPart::media("image/jpg", "aGVsbG8=");
        let block = lower(&part).unwrap();
        assert_eq!(
            serde_json::to_value(&block).unwrap(),
            json!({"image": {"format": "jpeg", "source": {"bytes": "aGVsbG8="}}}),
        );
    }

    #[test]
    fn lower_document_parts_default_the_name() {
        let part = ContentPart::media("application/pdf", "aGVsbG8=");
        let block = lower(&part).unwrap();
        assert_eq!(
            serde_json::to_value(&block).unwrap(),
            json!({
                "document": {
                    "format": "pdf",
                    "name": "document.pdf",
                    "source": {"bytes": "aGVsbG8="},
                },
            }),
        );
    }

    #[test]
    fn lower_document_parts_keep_the_filename() {
        let part = ContentPart::Media {
            media_type: "text/plain".to_string(),
            data: "aGVsbG8=".to_string(),
            filename: Some("notes.txt".to_string()),
            metadata: None,
        };
        let block = lower(&part).unwrap();
        assert_eq!(
            serde_json::to_value(&block).unwrap(),
            json!({
                "document": {
                    "format": "txt",
                    "name": "notes.txt",
                    "source": {"bytes": "aGVsbG8="},
                },
            }),
        );
    }

    #[test]
    fn lower_rejects_unsupported_image_mimes() {
        let part = ContentPart::media("image/svg+xml", "aGVsbG8=");
        let error = lower(&part).unwrap_err();
        assert!(reason_message(&error).contains("does not support image media type"),);
    }

    #[test]
    fn lower_rejects_unsupported_media_types() {
        let part = ContentPart::media("video/mp4", "aGVsbG8=");
        let error = lower(&part).unwrap_err();
        assert!(reason_message(&error).contains("does not support media type"),);
    }

    #[test]
    fn lower_rejects_mismatched_data_urls() {
        let part = ContentPart::media("image/png", "data:image/jpeg;base64,aGVsbG8=");
        let error = lower(&part).unwrap_err();
        assert!(reason_message(&error).contains("does not match data URL type"),);
    }

    #[test]
    fn lower_rejects_non_canonical_base64() {
        // "YR==" decodes to "a", which re-encodes as "YQ==" — not canonical.
        let part = ContentPart::media("image/png", "YR==");
        let error = lower(&part).unwrap_err();
        assert!(reason_message(&error).contains("canonical base64"));
    }

    #[test]
    fn lower_rejects_invalid_base64() {
        let part = ContentPart::media("image/png", "!!!not-base64!!!");
        let error = lower(&part).unwrap_err();
        assert!(reason_message(&error).contains("valid base64"));
    }

    #[test]
    fn lower_rejects_empty_data() {
        let part = ContentPart::media("image/png", "");
        let error = lower(&part).unwrap_err();
        assert!(reason_message(&error).contains("valid base64"));
    }
}

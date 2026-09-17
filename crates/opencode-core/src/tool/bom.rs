//! BOM helpers — port of `util/bom.ts` (spec M4.3).

use std::path::Path;

use crate::tool::error::ToolError;

const BOM: char = '\u{feff}';

/// `Bom.split` (bom.ts:7-10): strips a leading U+FEFF.
pub fn split(text: &str) -> Split {
    if text.starts_with(BOM) {
        Split {
            bom: true,
            text: text[BOM.len_utf8()..].to_string(),
        }
    } else {
        Split {
            bom: false,
            text: text.to_string(),
        }
    }
}

/// `Bom.split` result (`{ bom, text }`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Split {
    pub bom: bool,
    pub text: String,
}

/// `Bom.join` (bom.ts:12-16): re-adds the BOM (stripping first).
pub fn join(text: &str, bom: bool) -> String {
    let stripped = split(text).text;
    if bom {
        format!("{BOM}{stripped}")
    } else {
        stripped
    }
}

/// `Bom.readFile` (bom.ts:18-20): reads the file (utf-8, lossy like
/// `TextDecoder`) and splits off the BOM.
pub async fn read_file(file_path: &Path) -> Result<Split, ToolError> {
    let bytes = tokio::fs::read(file_path)
        .await
        .map_err(|e| ToolError::Failed(e.to_string()))?;
    Ok(split(&String::from_utf8_lossy(bytes.as_slice())))
}

/// `writeWithDirs` (fs-util.ts:127): `create_dir_all(parent)` then write
/// (spec §2.5).
pub async fn write_with_dirs(file_path: &Path, contents: &str) -> Result<(), ToolError> {
    if let Some(parent) = file_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| ToolError::Failed(e.to_string()))?;
    }
    tokio::fs::write(file_path, contents)
        .await
        .map_err(|e| ToolError::Failed(e.to_string()))
}

/// `Bom.syncFile` (bom.ts:22-27): re-writes the file when its BOM-ness
/// differs from `bom`; returns the current (post-format) text either way.
pub async fn sync_file(file_path: &Path, bom: bool) -> Result<String, ToolError> {
    let current = read_file(file_path).await?;
    if current.bom == bom {
        return Ok(current.text);
    }
    write_with_dirs(file_path, &join(&current.text, bom)).await?;
    Ok(current.text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_support::TempDir;

    #[test]
    fn split_strips_leading_bom_only() {
        let s = split("\u{feff}hello");
        assert!(s.bom);
        assert_eq!(s.text, "hello");
        let s = split("hello\u{feff}");
        assert!(!s.bom);
        assert_eq!(s.text, "hello\u{feff}");
        let s = split("");
        assert!(!s.bom);
        assert_eq!(s.text, "");
    }

    #[test]
    fn join_strips_then_adds() {
        assert_eq!(join("\u{feff}hi", true), "\u{feff}hi");
        assert_eq!(join("\u{feff}hi", false), "hi");
        assert_eq!(join("hi", true), "\u{feff}hi");
        assert_eq!(join("hi", false), "hi");
    }

    #[tokio::test]
    async fn read_file_and_sync_file_round_trip() {
        let temp = TempDir::new("bom-round");
        let path = temp.path().join("f.txt");

        std::fs::write(&path, "\u{feff}content\n").unwrap();
        let read = read_file(&path).await.unwrap();
        assert!(read.bom);
        assert_eq!(read.text, "content\n");

        // BOM-ness differs -> re-written without BOM.
        let text = sync_file(&path, false).await.unwrap();
        assert_eq!(text, "content\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "content\n");

        // Already synced -> no write, text returned.
        let text = sync_file(&path, true).await.unwrap();
        assert_eq!(text, "content\n");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "\u{feff}content\n");
        std::fs::remove_dir_all(temp.path()).ok();
    }
}

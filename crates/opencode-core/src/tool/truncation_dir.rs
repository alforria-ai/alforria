//! `TRUNCATION_DIR` — port of `tool/truncation-dir.ts`.
//!
//! TS computes the path once at module load; the Rust port resolves it on
//! call (`Global.Path.data/tool-output`, reusing the M3 `GlobalPaths`).

use std::path::PathBuf;

use crate::paths::GlobalPaths;

/// `<Global data dir>/tool-output`.
pub fn truncation_dir() -> PathBuf {
    GlobalPaths::from_env().data.join("tool-output")
}

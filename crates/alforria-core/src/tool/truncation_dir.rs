//! `TRUNCATION_DIR` — port of `tool/truncation-dir.ts`.
//!
//! TS computes the path once at module load from the process's global
//! paths; the Rust port takes the paths of the context it runs in, so a
//! context built over temporary directories writes there.

use std::path::PathBuf;

use crate::paths::GlobalPaths;

/// `<Global data dir>/tool-output`.
pub fn truncation_dir(paths: &GlobalPaths) -> PathBuf {
    paths.data.join("tool-output")
}

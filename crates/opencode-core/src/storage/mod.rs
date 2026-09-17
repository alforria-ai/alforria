//! SQLite storage: connection handling, migrations, schema.

pub mod connection;
pub mod migration;
pub mod schema;

pub use connection::{db_path, Storage};
pub use migration::{apply, apply_only, MIGRATION_IDS};
pub use schema::{
    Account, AccountState, ControlAccount, Message, Part, Project, Session, SessionShare, Todo,
    Workspace, SCHEMA_SQL,
};

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Minimal tempdir for storage tests (no `tempfile` dependency — spec
    /// §9 S8 limits new deps). Best-effort cleanup on drop.
    pub struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        pub fn new(tag: &str) -> TempDir {
            let path = std::env::temp_dir().join(format!(
                "opencode-core-storage-{tag}-{}-{}",
                std::process::id(),
                COUNTER.fetch_add(1, Ordering::Relaxed),
            ));
            std::fs::create_dir_all(&path).expect("create temp dir");
            TempDir { path }
        }

        pub fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

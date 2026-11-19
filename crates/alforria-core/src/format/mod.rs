//! `Format.Service` seam (format/index.ts). The full formatter port (formatter
//! discovery + external process spawning) is deferred; M4 gets the trait and
//! a no-op default (spec M4 module map).

use crate::tool::def::BoxFuture;

pub trait Formatter: Send + Sync {
    /// Runs the configured formatters for this file's extension. Returns
    /// whether any formatter ran (`Format.Interface.file`,
    /// format/index.ts:188-191). A `true` result makes the callers re-sync
    /// the file (BOM + re-read).
    fn file<'a>(&'a self, filepath: &'a str) -> BoxFuture<'a, bool>;
}

/// The M4 default: no formatters configured (`cfg.formatter` disabled).
pub struct NoopFormatter;

impl Formatter for NoopFormatter {
    fn file<'a>(&'a self, _filepath: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async move { false })
    }
}

/// A formatter that is not run on files in the excluded directories
/// (alforria extension, [`crate::lsp::Excluded`]): a formatter takes its
/// config — `prettier.config.js`, which is code — from beside the file it
/// formats, and a file in the upload area of a server with tokens below
/// `admin` was put there by somebody with no right to configure one.
/// Whoever installs a formatter on a server wraps it in this.
pub struct Excluding {
    pub inner: std::sync::Arc<dyn Formatter>,
    pub excluded: crate::lsp::Excluded,
}

impl Formatter for Excluding {
    fn file<'a>(&'a self, filepath: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            if crate::lsp::is_excluded(&Some(self.excluded.clone()), filepath) {
                return false;
            }
            self.inner.file(filepath).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct Recording(Mutex<Vec<String>>);

    impl Formatter for Recording {
        fn file<'a>(&'a self, filepath: &'a str) -> BoxFuture<'a, bool> {
            self.0.lock().unwrap().push(filepath.to_string());
            Box::pin(async move { true })
        }
    }

    /// A `prettier.config.js` under `uploads/` is never loaded: no
    /// formatter is run on a file there, so none looks beside it.
    #[tokio::test]
    async fn no_formatter_runs_on_a_file_in_an_excluded_directory() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("uploads/pkg")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("uploads/prettier.config.js"), "process.exit(1)").unwrap();
        for file in ["uploads/a.js", "uploads/pkg/b.js", "src/c.js"] {
            std::fs::write(root.join(file), "x").unwrap();
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.join("uploads"), root.join("alias")).unwrap();
        let recording = Arc::new(Recording(Mutex::new(Vec::new())));
        let uploads = root.join("uploads");
        let formatter = Excluding {
            inner: recording.clone(),
            excluded: Arc::new(move || vec![uploads.clone()]),
        };
        let path = |file: &str| root.join(file).to_string_lossy().into_owned();
        assert!(!formatter.file(&path("uploads/a.js")).await);
        assert!(!formatter.file(&path("uploads/pkg/b.js")).await);
        assert!(!formatter.file(&path("uploads/prettier.config.js")).await);
        #[cfg(unix)]
        assert!(!formatter.file(&path("alias/a.js")).await);
        assert!(formatter.file(&path("src/c.js")).await);
        assert_eq!(*recording.0.lock().unwrap(), vec![path("src/c.js")]);
    }
}

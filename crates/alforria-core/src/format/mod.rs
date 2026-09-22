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

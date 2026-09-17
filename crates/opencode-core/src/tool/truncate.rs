//! `Truncate.Service` — port of `tool/truncate.ts` (spec M4.1).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use opencode_schema::permission_v1::PermissionV1Action;
use ulid::Ulid;

use crate::tool::def::{AgentInfo, BoxFuture};
use crate::tool::permission::evaluate;

pub const MAX_LINES: usize = 2000;
pub const MAX_BYTES: usize = 50 * 1024; // 50 KB
/// Retention of `TRUNCATION_DIR` spill files (7 days; truncate.ts:12).
pub const RETENTION: Duration = Duration::new(7 * 24 * 3600, 0);

/// Truncation direction.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Direction {
    #[default]
    Head,
    Tail,
}

/// Per-call options (`maxLines`/`maxBytes`/`direction`, truncate.ts:21-25).
#[derive(Debug, Clone, Default)]
pub struct Options {
    pub max_lines: Option<usize>,
    pub max_bytes: Option<usize>,
    pub direction: Direction,
}

/// TS `Truncate.Result` (truncate.ts:19).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TruncResult {
    Unchanged {
        content: String,
    },
    Truncated {
        content: String,
        output_path: PathBuf,
    },
}

/// The `Truncate.Interface` service seam (truncate.ts:32-44).
pub trait Truncate: Send + Sync {
    /// Deletes TRUNCATION_DIR entries starting with `tool_` older than
    /// [`RETENTION`]. Errors are swallowed (missing dir -> no-op).
    fn cleanup(&self) -> BoxFuture<'static, ()>;
    /// Writes text to `TRUNCATION_DIR/tool_{ULID}`, creating the dir.
    /// Failures die (`Effect.orDie`).
    fn write<'a>(&'a self, text: &'a str) -> BoxFuture<'a, PathBuf>;
    /// Resolved truncation limits: config `tool_output.{max_lines,max_bytes}`
    /// ?? (MAX_LINES, MAX_BYTES).
    fn limits(&self) -> BoxFuture<'static, (usize, usize)>;
    /// Returns output unchanged when it fits within the limits, otherwise
    /// writes the full text to the truncation directory and returns a
    /// preview plus a hint to inspect the saved file.
    fn output<'a>(
        &'a self,
        text: &'a str,
        opts: Options,
        agent: Option<&'a AgentInfo>,
    ) -> BoxFuture<'a, TruncResult>;
}

/// `hasTaskTool` (truncate.ts:27-30): agents whose ruleset does not deny the
/// `task` tool get the "delegate to explore agent" hint variant.
fn has_task_tool(agent: Option<&AgentInfo>) -> bool {
    match agent {
        None => false,
        Some(a) => evaluate("task", "*", &[&a.permission]).action != PermissionV1Action::Deny,
    }
}

/// Production implementation over the local filesystem. The truncation
/// directory and limits are resolved at construction time; the clock is
/// injectable for the cleanup tests.
pub struct TruncateService {
    dir: PathBuf,
    max_lines: usize,
    max_bytes: usize,
    now: Arc<dyn Fn() -> SystemTime + Send + Sync>,
}

impl TruncateService {
    /// Service with resolved `tool_output` limits and the system clock.
    pub fn new(dir: PathBuf, max_lines: usize, max_bytes: usize) -> Self {
        TruncateService {
            dir,
            max_lines,
            max_bytes,
            now: Arc::new(SystemTime::now),
        }
    }

    /// Service with the default `MAX_LINES`/`MAX_BYTES` limits.
    pub fn default_limits(dir: PathBuf) -> Self {
        Self::new(dir, MAX_LINES, MAX_BYTES)
    }

    /// Test seam: deterministic clock for the 7-day retention filter.
    pub fn with_clock(
        dir: PathBuf,
        max_lines: usize,
        max_bytes: usize,
        now: Box<dyn Fn() -> SystemTime + Send + Sync>,
    ) -> Self {
        TruncateService {
            dir,
            max_lines,
            max_bytes,
            now: Arc::from(now),
        }
    }
}

impl Truncate for TruncateService {
    fn cleanup(&self) -> BoxFuture<'static, ()> {
        let dir = self.dir.clone();
        let now = Arc::clone(&self.now);
        Box::pin(async move {
            let entries = match std::fs::read_dir(&dir) {
                Ok(entries) => entries,
                Err(_) => return, // Effect.catch(() => succeed([]))
            };
            let cutoff = now() - RETENTION;
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !name.starts_with("tool_") {
                    continue;
                }
                let Ok(metadata) = entry.metadata() else {
                    continue;
                };
                let Ok(mtime) = metadata.modified() else {
                    continue;
                };
                if mtime >= cutoff {
                    continue;
                }
                let _ = std::fs::remove_file(entry.path());
            }
        })
    }

    fn write<'a>(&'a self, text: &'a str) -> BoxFuture<'a, PathBuf> {
        Box::pin(async move {
            let file = self.dir.join(format!("tool_{}", Ulid::new()));
            tokio::fs::create_dir_all(&self.dir)
                .await
                .expect("create truncation dir");
            tokio::fs::write(&file, text)
                .await
                .expect("write spill file");
            file
        })
    }

    fn limits(&self) -> BoxFuture<'static, (usize, usize)> {
        let (max_lines, max_bytes) = (self.max_lines, self.max_bytes);
        Box::pin(async move { (max_lines, max_bytes) })
    }

    fn output<'a>(
        &'a self,
        text: &'a str,
        opts: Options,
        agent: Option<&'a AgentInfo>,
    ) -> BoxFuture<'a, TruncResult> {
        Box::pin(async move {
            let (resolved_lines, resolved_bytes) = self.limits().await;
            let max_lines = opts.max_lines.unwrap_or(resolved_lines);
            let max_bytes = opts.max_bytes.unwrap_or(resolved_bytes);
            let direction = opts.direction;

            let lines: Vec<&str> = text.split('\n').collect();
            let total_bytes = text.len();

            if lines.len() <= max_lines && total_bytes <= max_bytes {
                return TruncResult::Unchanged {
                    content: text.to_string(),
                };
            }

            let mut out: Vec<&str> = Vec::new();
            let mut bytes = 0usize;
            let mut hit_bytes = false;

            match direction {
                Direction::Head => {
                    for (i, line) in lines.iter().enumerate() {
                        if i >= max_lines {
                            break;
                        }
                        let size = line.len() + usize::from(i > 0);
                        if bytes + size > max_bytes {
                            hit_bytes = true;
                            break;
                        }
                        out.push(line);
                        bytes += size;
                    }
                }
                Direction::Tail => {
                    for i in (0..lines.len()).rev() {
                        if out.len() >= max_lines {
                            break;
                        }
                        let size = lines[i].len() + usize::from(!out.is_empty());
                        if bytes + size > max_bytes {
                            hit_bytes = true;
                            break;
                        }
                        out.insert(0, lines[i]);
                        bytes += size;
                    }
                }
            }

            let removed = if hit_bytes {
                total_bytes - bytes
            } else {
                lines.len() - out.len()
            };
            let unit = if hit_bytes { "bytes" } else { "lines" };
            let preview = out.join("\n");
            let output_path = self.write(text).await;
            let file = output_path.display();

            let hint = if has_task_tool(agent) {
                format!(
                    "The tool call succeeded but the output was truncated. Full output saved to: {file}\nUse the Task tool to have explore agent process this file with Grep and Read (with offset/limit). Do NOT read the full file yourself - delegate to save context."
                )
            } else {
                format!(
                    "The tool call succeeded but the output was truncated. Full output saved to: {file}\nUse Grep to search the full content or Read with offset/limit to view specific sections."
                )
            };

            let content = match direction {
                Direction::Head => {
                    format!("{preview}\n\n...{removed} {unit} truncated...\n\n{hint}")
                }
                Direction::Tail => {
                    format!("...{removed} {unit} truncated...\n\n{hint}\n\n{preview}")
                }
            };
            TruncResult::Truncated {
                content,
                output_path,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use crate::tool::permission::Ruleset;
    use opencode_schema::permission_v1::{PermissionV1Action, PermissionV1Rule};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "opencode-truncate-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn agent(permission: Ruleset) -> Option<AgentInfo> {
        Some(AgentInfo {
            name: "build".to_string(),
            description: None,
            mode: crate::tool::def::AgentMode::Primary,
            permission,
        })
    }

    fn deny_rule() -> PermissionV1Rule {
        PermissionV1Rule {
            permission: "task".to_string(),
            pattern: "*".to_string(),
            action: PermissionV1Action::Deny,
        }
    }

    #[tokio::test]
    async fn output_under_limits_is_unchanged() {
        let dir = temp();
        let svc = TruncateService::default_limits(dir.clone());
        let result = svc.output("hello\nworld", Options::default(), None).await;
        assert_eq!(
            result,
            TruncResult::Unchanged {
                content: "hello\nworld".to_string()
            }
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn output_head_truncation_by_lines() {
        let dir = temp();
        let svc = TruncateService::new(dir.clone(), 2, 50_000);
        let result = svc
            .output("one\ntwo\nthree\nfour\nfive", Options::default(), None)
            .await;
        match result {
            TruncResult::Truncated {
                content,
                output_path,
            } => {
                assert!(content.starts_with("one\ntwo\n\n...3 lines truncated..."));
                assert!(content.contains("Full output saved to: "));
                assert!(content.ends_with("view specific sections."));
                // Spill file has the FULL text
                assert_eq!(
                    std::fs::read_to_string(&output_path).unwrap(),
                    "one\ntwo\nthree\nfour\nfive"
                );
                assert!(output_path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("tool_"));
            }
            _ => panic!("expected truncated"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn output_tail_truncation() {
        let dir = temp();
        let svc = TruncateService::new(dir.clone(), 2, 50_000);
        let result = svc
            .output(
                "one\ntwo\nthree\nfour\nfive",
                Options {
                    direction: Direction::Tail,
                    ..Default::default()
                },
                None,
            )
            .await;
        match result {
            TruncResult::Truncated { content, .. } => {
                assert!(content.starts_with("...3 lines truncated..."));
                assert!(content.contains("view specific sections.\n\nfour\nfive"));
            }
            _ => panic!("expected truncated"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn output_byte_hit_counts_bytes() {
        let dir = temp();
        // 3 lines of 20 bytes each = 60 bytes + newlines; byte budget 25.
        let svc = TruncateService::new(dir.clone(), 10, 25);
        let text = "aaaaaaaaaaaaaaaaaaaa\nbbbbbbbbbbbbbbbbbbbb\ncccccccccccccccccccc";
        let result = svc.output(text, Options::default(), None).await;
        match result {
            TruncResult::Truncated { content, .. } => {
                // total = 62 bytes; head keeps line 0 (20 bytes, no leading
                // newline counted), line 1 would exceed the 25-byte budget:
                // removed = 62 - 20 = 42.
                assert!(content.starts_with("aaaaaaaaaaaaaaaaaaaa\n\n...42 bytes truncated..."));
                assert!(!content.contains("lines truncated..."));
            }
            _ => panic!("expected truncated"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn output_task_hint_variant() {
        let dir = temp();
        let svc = TruncateService::new(dir.clone(), 1, 50_000);
        let a = agent(vec![]);
        let result = svc.output("a\nb\nc", Options::default(), a.as_ref()).await;
        match result {
            TruncResult::Truncated { content, .. } => {
                assert!(content.contains("Use the Task tool to have explore agent process this file with Grep and Read (with offset/limit). Do NOT read the full file yourself - delegate to save context."));
            }
            _ => panic!("expected truncated"),
        }
        // Denying task falls back to the plain variant.
        let result = svc
            .output(
                "a\nb\nc",
                Options::default(),
                agent(vec![deny_rule()]).as_ref(),
            )
            .await;
        match result {
            TruncResult::Truncated { content, .. } => {
                assert!(content.contains("Use Grep to search the full content or Read with offset/limit to view specific sections."));
            }
            _ => panic!("expected truncated"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn cleanup_removes_only_stale_tool_files() {
        let dir = temp();
        let svc = TruncateService::with_clock(
            dir.clone(),
            MAX_LINES,
            MAX_BYTES,
            Box::new(SystemTime::now),
        );
        // Fresh files (mtime now >= cutoff) are kept.
        svc.write("hello").await;
        std::fs::write(dir.join("other_file"), "hello").unwrap();
        svc.cleanup().await;
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
        std::fs::remove_dir_all(&dir).ok();

        // With a clock 8 days ahead, real-mtime files are stale: all
        // tool_-prefixed files are removed, others survive.
        let dir = temp();
        let svc = TruncateService::with_clock(
            dir.clone(),
            MAX_LINES,
            MAX_BYTES,
            Box::new(|| SystemTime::now() + Duration::new(8 * 24 * 3600, 0)),
        );
        svc.write("hello").await;
        std::fs::write(dir.join("tool_manual"), "delete me").unwrap();
        std::fs::write(dir.join("other_file"), "keep me").unwrap();
        svc.cleanup().await;
        let mut remaining: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        remaining.sort();
        assert_eq!(remaining, vec!["other_file"]);
        std::fs::remove_dir_all(&dir).ok();
    }
}

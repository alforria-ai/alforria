//! External-directory guard — port of `tool/external-directory.ts` and the
//! `containsPath` helper from `project/instance-context.ts` (spec M4.1).

use std::path::Path;

use serde_json::json;

use crate::tool::def::{AskRequest, InstanceContext, ToolCtxRef};
use crate::tool::error::ToolError;

/// Guarded path kind — `type Kind = "file" | "directory"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Kind {
    #[default]
    File,
    Directory,
}

/// `type Options = { bypass?: boolean; kind?: Kind }` (defaults: no bypass,
/// `kind = "file"`).
#[derive(Debug, Clone, Copy, Default)]
pub struct ExternalOptions {
    /// `options.bypass` — skip the check entirely.
    pub bypass: bool,
    pub kind: Kind,
}

/// `FSUtil.contains(parent, child)` — true when `child` is `parent` itself or
/// lies inside it.
fn fs_contains(parent: &Path, child: &Path) -> bool {
    child.strip_prefix(parent).is_ok()
}

/// `containsPath` (instance-context.ts:18-24): true when the path is inside
/// `ctx.directory` or `ctx.worktree`. Non-git projects set worktree to `"/"`,
/// which would match ANY absolute path, so the worktree check is skipped.
pub fn contains_path(filepath: &Path, ctx: &InstanceContext) -> bool {
    if fs_contains(&ctx.directory, filepath) {
        return true;
    }
    if ctx.worktree == Path::new("/") {
        return false;
    }
    fs_contains(&ctx.worktree, filepath)
}

/// `path.dirname` semantics: the parent of a bare filename is `"."`.
fn dirname(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

/// `assertExternalDirectory` (external-directory.ts:15-45).
///
/// Returns `false` (no-op) when `target` is `None`, `options.bypass` is
/// set, or the path is inside the instance (`containsPath`). Otherwise asks
/// permission `"external_directory"` with `patterns = always =
/// ["{dir}/*"]` and metadata `{ filepath, parentDir }`, returning `true`.
pub async fn assert_external_directory(
    ctx: &ToolCtxRef<'_>,
    target: Option<&str>,
    options: ExternalOptions,
) -> Result<bool, ToolError> {
    let Some(target) = target else {
        return Ok(false);
    };

    if options.bypass {
        return Ok(false);
    }

    let instance = ctx.instance;
    let full = Path::new(target);
    if contains_path(full, instance) {
        return Ok(false);
    }

    let dir = match options.kind {
        Kind::Directory => full,
        Kind::File => dirname(full),
    };
    let glob = dir.join("*").to_string_lossy().replace('\\', "/");

    ctx.ask
        .ask(AskRequest {
            permission: "external_directory".to_string(),
            patterns: vec![glob.clone()],
            always: vec![glob],
            metadata: json!({
                "filepath": target,
                "parentDir": dir.to_string_lossy().to_string(),
            }),
        })
        .await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::tool::def::{Ask, BoxFuture, Extra, MetadataInput, MetadataSink};

    struct RecordingAsk {
        requests: Mutex<Vec<AskRequest>>,
    }

    impl RecordingAsk {
        fn new() -> Self {
            RecordingAsk {
                requests: Mutex::new(Vec::new()),
            }
        }
    }

    impl Ask for RecordingAsk {
        fn ask<'a>(&'a self, request: AskRequest) -> BoxFuture<'a, Result<(), ToolError>> {
            Box::pin(async move {
                self.requests.lock().unwrap().push(request);
                Ok(())
            })
        }
    }

    impl MetadataSink for RecordingAsk {
        fn metadata<'a>(&'a self, _input: MetadataInput) -> BoxFuture<'a, Result<(), ToolError>> {
            Box::pin(async move { Ok(()) })
        }
    }

    fn ctx<'a>(
        ask: &'a RecordingAsk,
        instance: &'a InstanceContext,
        extra: &'a Extra,
    ) -> ToolCtxRef<'a> {
        ToolCtxRef {
            session_id: "ses_1",
            message_id: "msg_1",
            agent: "build",
            call_id: None,
            abort: tokio_util::sync::CancellationToken::new(),
            messages: &[],
            extra,
            instance,
            ask,
            metadata: ask,
        }
    }

    fn instance() -> InstanceContext {
        InstanceContext {
            directory: Path::new("/home/u/project").to_path_buf(),
            worktree: Path::new("/home/u/project").to_path_buf(),
        }
    }

    #[test]
    fn contains_path_boundary() {
        let ctx = InstanceContext {
            directory: Path::new("/home/u/project").to_path_buf(),
            worktree: Path::new("/home/u/wt").to_path_buf(),
        };
        assert!(contains_path(Path::new("/home/u/project"), &ctx));
        assert!(contains_path(
            Path::new("/home/u/project/src/main.rs"),
            &ctx
        ));
        assert!(contains_path(Path::new("/home/u/wt/other"), &ctx));
        assert!(!contains_path(Path::new("/home/u/projectx/file"), &ctx));
        // "/" worktree never matches (non-git quirk).
        let non_git = InstanceContext {
            directory: Path::new("/home/u/project").to_path_buf(),
            worktree: Path::new("/").to_path_buf(),
        };
        assert!(!contains_path(Path::new("/etc/passwd"), &non_git));
    }

    #[tokio::test]
    async fn inside_instance_is_a_no_op() {
        let ask = RecordingAsk::new();
        let instance = instance();
        let extra = Extra::default();
        let c = ctx(&ask, &instance, &extra);
        let asked = assert_external_directory(
            &c,
            Some("/home/u/project/src/main.rs"),
            ExternalOptions::default(),
        )
        .await
        .unwrap();
        assert!(!asked);
        assert!(ask.requests.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn outside_instance_asks_with_dir_glob() {
        let ask = RecordingAsk::new();
        let instance = instance();
        let extra = Extra::default();
        let c = ctx(&ask, &instance, &extra);
        let asked = assert_external_directory(
            &c,
            Some("/tmp/outside/file.txt"),
            ExternalOptions::default(),
        )
        .await
        .unwrap();
        assert!(asked);
        let requests = ask.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].permission, "external_directory");
        assert_eq!(requests[0].patterns, vec!["/tmp/outside/*".to_string()]);
        assert_eq!(requests[0].always, vec!["/tmp/outside/*".to_string()]);
        assert_eq!(
            requests[0].metadata,
            serde_json::json!({
                "filepath": "/tmp/outside/file.txt",
                "parentDir": "/tmp/outside",
            })
        );
    }

    #[tokio::test]
    async fn directory_kind_asks_on_the_dir_itself() {
        let ask = RecordingAsk::new();
        let instance = instance();
        let extra = Extra::default();
        let c = ctx(&ask, &instance, &extra);
        let asked = assert_external_directory(
            &c,
            Some("/tmp/outside/dir"),
            ExternalOptions {
                bypass: false,
                kind: Kind::Directory,
            },
        )
        .await
        .unwrap();
        assert!(asked);
        let requests = ask.requests.lock().unwrap();
        assert_eq!(requests[0].patterns, vec!["/tmp/outside/dir/*".to_string()]);
        assert_eq!(
            requests[0].metadata["parentDir"],
            serde_json::json!("/tmp/outside/dir")
        );
    }

    #[tokio::test]
    async fn bypass_and_none_target_short_circuit() {
        let ask = RecordingAsk::new();
        let instance = instance();
        let extra = Extra::default();
        let c = ctx(&ask, &instance, &extra);
        assert!(
            !assert_external_directory(&c, None, ExternalOptions::default())
                .await
                .unwrap()
        );
        assert!(!assert_external_directory(
            &c,
            Some("/tmp/outside/file.txt"),
            ExternalOptions {
                bypass: true,
                ..Default::default()
            },
        )
        .await
        .unwrap());
        assert!(ask.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn dirname_fallback_for_relative_paths() {
        assert_eq!(dirname(Path::new("file.txt")), Path::new("."));
        assert_eq!(dirname(Path::new("/a/b.txt")), Path::new("/a"));
        assert_eq!(dirname(Path::new("/file.txt")), Path::new("/"));
    }
}

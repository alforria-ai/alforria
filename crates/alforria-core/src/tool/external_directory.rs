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

/// Where `path` really is: symlinks resolved through the longest part of it
/// that exists, the rest appended as written. A link whose target is missing
/// is followed too, since a write through it creates the target.
pub fn real_path(path: &Path) -> std::path::PathBuf {
    let mut current = crate::git::lexical_normalize(path);
    // Bounds a symlink loop.
    for _ in 0..40 {
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        let mut probe = current.clone();
        let dangling = loop {
            if let Ok(real) = std::fs::canonicalize(&probe) {
                let mut out = real;
                out.extend(tail.iter().rev());
                return out;
            }
            if std::fs::symlink_metadata(&probe).is_ok_and(|meta| meta.file_type().is_symlink()) {
                break Some(probe);
            }
            match (probe.parent(), probe.file_name()) {
                (Some(parent), Some(name)) => {
                    tail.push(name.to_os_string());
                    probe = parent.to_path_buf();
                }
                _ => break None,
            }
        };
        let Some(link) = dangling else {
            return current;
        };
        let Ok(target) = std::fs::read_link(&link) else {
            return current;
        };
        let mut next = link.parent().unwrap_or(Path::new("/")).join(target);
        next.extend(tail.iter().rev());
        current = crate::git::lexical_normalize(&next);
    }
    current
}

/// Whether `child`, already resolved, lies in `root` or in where `root`
/// really is (a project reached through a symlinked path).
fn real_contains(root: &Path, child: &Path) -> bool {
    fs_contains(root, child)
        || std::fs::canonicalize(root).is_ok_and(|real| fs_contains(&real, child))
}

/// `containsPath` (instance-context.ts:18-24): true when the path is inside
/// `ctx.directory` or `ctx.worktree`. Non-git projects set worktree to `"/"`,
/// which would match ANY absolute path, so the worktree check is skipped.
///
/// The path is normalised lexically first: `strip_prefix` compares
/// components literally, so `/project/../etc/passwd` would otherwise count
/// as inside `/project`. It must also really be inside: a symlink in the
/// project that points out of it is outside, whatever its spelling.
pub fn contains_path(filepath: &Path, ctx: &InstanceContext) -> bool {
    let filepath = &crate::git::lexical_normalize(filepath);
    let git = ctx.worktree != Path::new("/");
    let lexical =
        fs_contains(&ctx.directory, filepath) || (git && fs_contains(&ctx.worktree, filepath));
    if !lexical {
        return false;
    }
    let real = &real_path(filepath);
    real == filepath
        || real_contains(&ctx.directory, real)
        || (git && real_contains(&ctx.worktree, real))
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
    // Normalise so the prompt and the saved `always` rule name the directory
    // actually touched, not a `..`-laden spelling of it.
    let normalized = if Path::new(target).is_absolute() {
        crate::git::lexical_normalize(Path::new(target))
    } else {
        Path::new(target).to_path_buf()
    };
    if contains_path(&normalized, instance) {
        return Ok(false);
    }
    // Ask about the directory actually touched: for a symlink that leaves
    // the project, that is where it points.
    let lexically_inside = {
        let git = instance.worktree != Path::new("/");
        fs_contains(&instance.directory, &normalized)
            || (git && fs_contains(&instance.worktree, &normalized))
    };
    let resolved = if lexically_inside {
        real_path(&normalized)
    } else {
        normalized
    };
    let full = resolved.as_path();

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
        // `..` must not smuggle an outside path past the prefix check.
        assert!(!contains_path(
            Path::new("/home/u/project/../../etc/passwd"),
            &ctx
        ));
        assert!(!contains_path(Path::new("/home/u/wt/../secret"), &ctx));
        assert!(contains_path(
            Path::new("/home/u/project/src/../Cargo.toml"),
            &ctx
        ));
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

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_out_of_the_project_is_outside() {
        let root = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(root.path()).unwrap();
        let project = root.join("project");
        let outside = root.join("outside");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret"), "s").unwrap();
        std::os::unix::fs::symlink(outside.join("secret"), project.join("notes.txt")).unwrap();
        std::os::unix::fs::symlink(&outside, project.join("docs")).unwrap();
        // A link to a file that does not exist yet: writing creates it.
        std::os::unix::fs::symlink(outside.join("new"), project.join("dangling")).unwrap();
        std::fs::write(project.join("real.txt"), "r").unwrap();
        std::os::unix::fs::symlink(project.join("real.txt"), project.join("alias")).unwrap();

        let instance = InstanceContext {
            directory: project.clone(),
            worktree: project.clone(),
        };
        let extra = Extra::default();
        let glob = format!("{}/*", outside.display());
        for name in ["notes.txt", "docs/secret", "docs/not-there", "dangling"] {
            let ask = RecordingAsk::new();
            let c = ctx(&ask, &instance, &extra);
            let target = project.join(name);
            let asked = assert_external_directory(
                &c,
                Some(target.to_string_lossy().as_ref()),
                ExternalOptions::default(),
            )
            .await
            .unwrap();
            assert!(asked, "{name}");
            assert_eq!(
                ask.requests.lock().unwrap()[0].patterns,
                vec![glob.clone()],
                "{name}"
            );
        }
        // Links that stay inside, and plain files, are still inside.
        for name in ["alias", "real.txt", "new-file", "sub/new-file"] {
            assert!(contains_path(&project.join(name), &instance), "{name}");
        }
    }

    #[tokio::test]
    async fn dotdot_escape_asks_with_the_real_directory() {
        let ask = RecordingAsk::new();
        let instance = instance();
        let extra = Extra::default();
        let c = ctx(&ask, &instance, &extra);
        let asked = assert_external_directory(
            &c,
            Some("/home/u/project/../../etc/passwd"),
            ExternalOptions::default(),
        )
        .await
        .unwrap();
        assert!(asked);
        let requests = ask.requests.lock().unwrap();
        assert_eq!(requests[0].patterns, vec!["/home/etc/*".to_string()]);
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

//! `{env:VAR}` / `{file:path}` substitution: port of
//! `packages/opencode/src/config/variable.ts`.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use regex::Regex;
use std::sync::LazyLock;

use crate::CoreError;

/// Where substituting text comes from — either a real config file on disk, or
/// a virtual source (env content, remote body) with a directory to resolve
/// relative `{file:...}` paths against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Path(PathBuf),
    Virtual { dir: PathBuf, source: String },
}

impl Source {
    fn dir(&self) -> PathBuf {
        match self {
            Source::Path(p) => p.parent().unwrap_or(Path::new("")).to_path_buf(),
            Source::Virtual { dir, .. } => dir.clone(),
        }
    }

    fn name(&self) -> PathBuf {
        match self {
            Source::Path(p) => p.clone(),
            Source::Virtual { source, .. } => PathBuf::from(source),
        }
    }
}

/// What to do when a `{file:...}` reference cannot be read
/// (TS `missing: "error" | "empty"`, defaulting to `"error"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Missing {
    Error,
    Empty,
}

static ENV_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{env:([^}]+)\}").unwrap());
static FILE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{file:[^}]+\}").unwrap());

/// What a config that is *not the user's own* may substitute.
///
/// `{env:NAME}` and `{file:path}` are upstream features written for the
/// user's config. A project's `opencode.json` — a file that arrives with a
/// clone and that the agent can write — and a remote config got them too,
/// and so could read the server's password, a provider key or `auth.json`
/// into a system prompt, an MCP server's environment or a base URL. For
/// such a source:
///
/// * `{env:NAME}` substitutes only when `env_allowed(NAME)`; a refused name
///   substitutes the empty string (as an unset variable does) and is logged;
/// * `{file:path}` must be a relative path that, with symlinks resolved,
///   stays inside `root`; an absolute path, `~`, a `..` or a link that
///   leaves it is a bad file reference. With no `root` (a remote config)
///   every `{file:}` is one.
pub struct Confinement<'a> {
    /// The project directory `{file:}` may read inside.
    pub root: Option<PathBuf>,
    /// Whether this source may read environment variable `NAME`.
    pub env_allowed: &'a dyn Fn(&str) -> bool,
}

/// Apply `{env:VAR}` and `{file:path}` substitutions to config text.
///
/// * `{env:VAR}` → the injected map's value for `VAR` falling back to the
///   process environment; an unset variable substitutes the empty string
///   (TS `|| ""` — it is not an error).
/// * `{file:path}` → the file's trimmed contents, JSON-string-escaped
///   (`JSON.stringify(content).slice(1, -1)` — the escaping matters). `~/`
///   expands to the home directory; relative paths resolve against the config
///   file's directory. Tokens on lines whose trimmed prefix starts with `//`
///   are left verbatim.
///
/// A `{file:...}` failure raises `CoreError::ConfigInvalid`
/// (`bad file reference: "{token}" ...`) unless `missing` is `Empty`.
///
/// This is the form for the user's own config: nothing is refused. A
/// project's or a remote config goes through [`substitute_confined`].
pub fn substitute(
    text: &str,
    source: &Source,
    env: &HashMap<String, String>,
    missing: Missing,
) -> Result<String, CoreError> {
    substitute_confined(text, source, env, missing, None)
}

/// [`substitute`], with what the source may read limited by `confinement`
/// (`None`: the user's own config, unlimited).
pub fn substitute_confined(
    text: &str,
    source: &Source,
    env: &HashMap<String, String>,
    missing: Missing,
    confinement: Option<&Confinement<'_>>,
) -> Result<String, CoreError> {
    let text = ENV_RE.replace_all(text, |captures: &regex::Captures| {
        let var = captures.get(1).unwrap().as_str();
        if let Some(confinement) = confinement {
            if !(confinement.env_allowed)(var) {
                tracing::warn!(
                    "ignoring {{env:{var}}} in {}: a project's or a remote config may not read \
                     that variable (see `server.trust.project_env` and `shell_env`)",
                    source.name().display()
                );
                return String::new();
            }
        }
        env.get(var)
            .cloned()
            .or_else(|| env::var(var).ok())
            .unwrap_or_default()
    });

    let mut out = String::new();
    let mut cursor = 0;

    let config_dir = source.dir();

    for m in FILE_RE.find_iter(&text) {
        let token = m.as_str();
        let index = m.start();

        out.push_str(&text[cursor..index]);

        // TS: text.lastIndexOf("\n", index - 1) + 1 — the start of the line
        // the token sits on.
        let line_start = text[..index].rfind('\n').map(|p| p + 1).unwrap_or(0);
        let prefix = text[line_start..index].trim_start();
        if prefix.starts_with("//") {
            out.push_str(token);
            cursor = index + token.len();
            continue;
        }

        let file_path = &token["{file:".len()..token.len() - 1];
        let resolved = match confinement {
            None => resolve_file_path(&config_dir, file_path),
            Some(confinement) => {
                match confined_file_path(&config_dir, file_path, confinement.root.as_deref()) {
                    Ok(resolved) => resolved,
                    // Refused whatever `missing` says: this is not a file
                    // that happens to be absent.
                    Err(reason) => {
                        return Err(CoreError::invalid(
                            source.name(),
                            format!("bad file reference: \"{token}\" {reason}"),
                        ));
                    }
                }
            }
        };

        match fs::read_to_string(&resolved) {
            Ok(content) => {
                out.push_str(&json_escape(content.trim()));
            }
            Err(err) => {
                if missing == Missing::Empty {
                    // TS: catch returns "" for every error when missing === "empty".
                    out.push_str("");
                } else {
                    let message = if err.kind() == ErrorKind::NotFound {
                        format!(
                            "bad file reference: \"{token}\" {} does not exist",
                            resolved.display()
                        )
                    } else {
                        format!("bad file reference: \"{token}\"")
                    };
                    return Err(CoreError::invalid(source.name(), message));
                }
            }
        }
        cursor = index + token.len();
    }

    out.push_str(&text[cursor..]);
    Ok(out)
}

/// `~/` expands to the home directory; absolute paths pass through; anything
/// else resolves against the config file's directory (like `path.resolve`:
/// made absolute and lexically normalized).
fn resolve_file_path(config_dir: &Path, file_path: &str) -> PathBuf {
    let joined = match file_path.strip_prefix("~/") {
        Some(rest) => match dirs::home_dir() {
            Some(home) => home.join(rest),
            None => PathBuf::from(file_path),
        },
        None => PathBuf::from(file_path),
    };

    if joined.is_absolute() {
        normalize(&joined)
    } else {
        let dir = if config_dir.is_absolute() {
            config_dir.to_path_buf()
        } else {
            env::current_dir()
                .map(|cwd| cwd.join(config_dir))
                .unwrap_or_else(|_| config_dir.to_path_buf())
        };
        normalize(&dir.join(joined))
    }
}

/// The file a confined `{file:path}` denotes, or why it is refused: the path
/// must be relative, and — with every symlink resolved — inside `root`.
fn confined_file_path(
    config_dir: &Path,
    file_path: &str,
    root: Option<&Path>,
) -> Result<PathBuf, String> {
    let Some(root) = root else {
        return Err("is not allowed in a remote config".to_string());
    };
    let outside = || {
        format!(
            "is outside the project: a project config reads files inside {} only",
            root.display()
        )
    };
    let path = Path::new(file_path);
    if file_path.starts_with('~') || path.is_absolute() || path.has_root() {
        return Err(outside());
    }
    let resolved = resolve_file_path(config_dir, file_path);
    // Both ends real: `..` is already folded, and a link that points out of
    // the project resolves to where it points. A file that does not exist
    // has nothing to resolve; its lexical path must be inside, and reading
    // it fails as a missing file.
    let real_root = fs::canonicalize(root).unwrap_or_else(|_| normalize(root));
    let inside = match fs::canonicalize(&resolved) {
        Ok(real) => real.starts_with(&real_root),
        Err(_) => resolved.starts_with(&real_root) || resolved.starts_with(normalize(root)),
    };
    if inside {
        Ok(resolved)
    } else {
        Err(outside())
    }
}

/// Lexical normalization (`path.resolve` never returns `..` segments).
fn normalize(path: &Path) -> PathBuf {
    let mut components = Vec::new();
    for component in path.components() {
        use std::path::Component::*;
        match component {
            Prefix(_) | RootDir => components.push(component),
            CurDir => {}
            ParentDir => match components.last() {
                Some(last) => match last {
                    std::path::Component::Prefix(_) | std::path::Component::RootDir => {}
                    _ => {
                        components.pop();
                    }
                },
                None => components.push(component),
            },
            Normal(_) => components.push(component),
        }
    }
    components.into_iter().collect()
}

/// `JSON.stringify(content).slice(1, -1)`: the string escaped as a JSON string
/// body (quotes stripped, so the escaped text is spliced inline).
fn json_escape(content: &str) -> String {
    match serde_json::to_string(content) {
        Ok(quoted) => quoted[1..quoted.len() - 1].to_owned(),
        Err(_) => content.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    fn env_map(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// Unique scratch directory under the OS temp dir (std-only `tempfile`
    /// replacement; S8 forbids new deps).
    fn temp_dir() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = env::temp_dir().join(format!(
            "alforria-core-m31-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed),
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn confined(
        text: &str,
        file: &Path,
        root: Option<&Path>,
        env_allowed: &dyn Fn(&str) -> bool,
    ) -> Result<String, CoreError> {
        substitute_confined(
            text,
            &Source::Path(file.to_path_buf()),
            &env_map(&[("FROM_MAP", "map-secret"), ("OK_NAME", "fine")]),
            Missing::Error,
            Some(&Confinement {
                root: root.map(Path::to_path_buf),
                env_allowed,
            }),
        )
    }

    /// A project config cannot read a variable it is not allowed, from the
    /// injected map or from the process; an allowed one still substitutes.
    #[test]
    fn a_confined_source_reads_only_allowed_variables() {
        let root = temp_dir();
        let file = root.join("opencode.json");
        let allowed = |name: &str| name == "OK_NAME";
        let out = confined(
            "a={env:FROM_MAP} b={env:PATH} c={env:OK_NAME}",
            &file,
            Some(&root),
            &allowed,
        )
        .unwrap();
        assert_eq!(out, "a= b= c=fine");
    }

    /// `{file:}` from a project config stays inside the project: no absolute
    /// path, no `~`, no `..`, no symlink out; a remote config has no files.
    #[test]
    fn a_confined_source_reads_files_inside_its_root_only() {
        let base = temp_dir();
        let root = base.join("project");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("inside.txt"), "inside").unwrap();
        fs::write(base.join("secret.txt"), "sk-secret").unwrap();
        let file = root.join("sub").join("opencode.json");
        let any = |_: &str| true;

        let ok = confined("{file:../inside.txt}", &file, Some(&root), &any).unwrap();
        assert_eq!(ok, "inside");

        let secret = base.join("secret.txt");
        for reference in [
            format!("{{file:{}}}", secret.display()),
            "{file:../../secret.txt}".to_string(),
            "{file:~/.config/alforria/opencode.json}".to_string(),
            "{file:~}".to_string(),
            // Absolute, even though it is inside.
            format!("{{file:{}}}", root.join("inside.txt").display()),
        ] {
            let err = confined(&reference, &file, Some(&root), &any).unwrap_err();
            assert!(
                err.to_string().contains("bad file reference"),
                "{reference}: {err}"
            );
            assert!(!err.to_string().contains("sk-secret"));
        }

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&secret, root.join("link.txt")).unwrap();
            std::os::unix::fs::symlink(&base, root.join("up")).unwrap();
            for reference in ["{file:../link.txt}", "{file:../up/secret.txt}"] {
                let err = confined(reference, &file, Some(&root), &any).unwrap_err();
                assert!(err.to_string().contains("outside the project"), "{err}");
            }
        }

        // `missing: Empty` does not turn a refusal into an empty string.
        let err = substitute_confined(
            "{file:../../secret.txt}",
            &Source::Path(file.clone()),
            &HashMap::new(),
            Missing::Empty,
            Some(&Confinement {
                root: Some(root.clone()),
                env_allowed: &any,
            }),
        )
        .unwrap_err();
        assert!(err.to_string().contains("outside the project"), "{err}");

        // A remote config: no root, no files.
        let err = confined("{file:inside.txt}", &file, None, &any).unwrap_err();
        assert!(err.to_string().contains("remote config"), "{err}");
    }

    #[test]
    fn substitutes_env_values() {
        let env = env_map(&[("MY_VAR", "hello")]);
        let out = substitute(
            "say {env:MY_VAR}",
            &Source::Virtual {
                dir: ".".into(),
                source: "test".into(),
            },
            &env,
            Missing::Error,
        );
        assert_eq!(out.unwrap(), "say hello");
    }

    #[test]
    fn unset_env_substitutes_empty_string() {
        let env = HashMap::new();
        let out = substitute(
            "say '{env:OPENCODE_M3_DEFINITELY_UNSET_VAR}'",
            &Source::Virtual {
                dir: ".".into(),
                source: "test".into(),
            },
            &env,
            Missing::Error,
        );
        assert_eq!(out.unwrap(), "say ''");
    }

    #[test]
    fn env_falls_back_to_process_env() {
        env::set_var("OPENCODE_M3_TEST_VAR", "from-process");
        let env = HashMap::new();
        let out = substitute(
            "{env:OPENCODE_M3_TEST_VAR}",
            &Source::Virtual {
                dir: ".".into(),
                source: "test".into(),
            },
            &env,
            Missing::Error,
        );
        env::remove_var("OPENCODE_M3_TEST_VAR");
        assert_eq!(out.unwrap(), "from-process");
    }

    #[test]
    fn substitutes_file_contents_escaped() {
        let dir = temp_dir();
        let secret = dir.join("secret.txt");
        fs::write(&secret, "  say \"hi\"\nnext\tline\n").unwrap();

        let source = Source::Path(dir.join("opencode.json"));
        let env = HashMap::new();
        let out = substitute(
            r#"token: "{file:secret.txt}""#,
            &source,
            &env,
            Missing::Error,
        );
        assert_eq!(out.unwrap(), r#"token: "say \"hi\"\nnext\tline""#);
    }

    #[test]
    fn file_resolution_is_relative_to_config_dir() {
        let dir = temp_dir();
        fs::write(dir.join("nested.txt"), "nested").unwrap();
        let source = Source::Virtual {
            dir: dir.to_path_buf(),
            source: "virtual".into(),
        };
        let env = HashMap::new();
        let out = substitute("{file:./nested.txt}", &source, &env, Missing::Error);
        assert_eq!(out.unwrap(), "nested");
    }

    #[test]
    fn comment_lines_keep_file_tokens_verbatim() {
        let dir = temp_dir();
        let env = HashMap::new();
        let source = Source::Virtual {
            dir: dir.to_path_buf(),
            source: "virtual".into(),
        };
        let text = "// keep {file:does-not-exist.txt}\n\"k\": \"v\"";
        let out = substitute(text, &source, &env, Missing::Error);
        assert_eq!(out.unwrap(), text);
    }

    #[test]
    fn missing_file_errors_by_default() {
        let dir = temp_dir();
        let source = Source::Path(dir.join("opencode.json"));
        let env = HashMap::new();
        let err = substitute("{file:nope.txt}", &source, &env, Missing::Error).unwrap_err();
        match err {
            CoreError::ConfigInvalid { path, message, .. } => {
                assert_eq!(path, source.name());
                let expected = format!(
                    "bad file reference: \"{{file:nope.txt}}\" {} does not exist",
                    dir.join("nope.txt").display()
                );
                assert_eq!(message.unwrap(), expected);
            }
            other => panic!("expected ConfigInvalid, got {other:?}"),
        }
    }

    #[test]
    fn missing_file_empty_mode_returns_empty() {
        let dir = temp_dir();
        let source = Source::Path(dir.join("opencode.json"));
        let env = HashMap::new();
        let out = substitute("x{file:nope.txt}y", &source, &env, Missing::Empty);
        assert_eq!(out.unwrap(), "xy");
    }
}

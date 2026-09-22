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
pub fn substitute(
    text: &str,
    source: &Source,
    env: &HashMap<String, String>,
    missing: Missing,
) -> Result<String, CoreError> {
    let text = ENV_RE.replace_all(text, |captures: &regex::Captures| {
        let var = captures.get(1).unwrap().as_str();
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
        let resolved = resolve_file_path(&config_dir, file_path);

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

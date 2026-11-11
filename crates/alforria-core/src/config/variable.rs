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
        // The user's own config reads the path as written. A confined
        // source is opened beneath its root in one resolution and read from
        // that descriptor (`crate::beneath`): there is no check on one path
        // and read of another.
        let (resolved, read) = match confinement {
            None => {
                let resolved = resolve_file_path(&config_dir, file_path);
                let read = fs::read_to_string(&resolved);
                (resolved, read)
            }
            Some(confinement) => {
                match confined_read(&config_dir, file_path, confinement.root.as_deref()) {
                    Ok(found) => found,
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

        match read {
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

/// `${VAR}` expansion in a provider base URL (provider.ts:1734-1741) — the
/// one implementation, for the loader and the provider runtime.
///
/// One pass over the text as written: a substituted value is never scanned
/// again. `allowed` is the decision `{env:NAME}` is under for the source the
/// text came from; a name it refuses is an `Err` carrying that name — the
/// caller turns it into an error, never into a URL. An allowed name that is
/// unset is left as written, as upstream does.
pub fn interpolate_dollar(
    text: &str,
    allowed: &dyn Fn(&str) -> bool,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<String, String> {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let Some(end) = rest.find('}') else {
            break;
        };
        let key = &rest[2..end];
        if !allowed(key) {
            return Err(key.to_string());
        }
        match lookup(key) {
            Some(value) => out.push_str(&value),
            None => out.push_str(&rest[..end + 1]),
        }
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// [`interpolate_dollar`] for a source that is not the host's, made final:
/// whatever `${` the result still holds — an unset reference, a value that
/// itself contains one, a `$${X}` whose expansion forms one — is written
/// `%24{`, the same URL text with nothing left that a later expansion (the
/// provider runtime's, which knows the host's rule only) would act on. The
/// expansion of such a URL happens here, once, under its source's rule.
pub fn interpolate_dollar_final(
    text: &str,
    allowed: &dyn Fn(&str) -> bool,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<String, String> {
    interpolate_dollar(text, allowed, lookup).map(|expanded| expanded.replace("${", "%24{"))
}

/// Which strings the *host* wrote: the only ones a `${VAR}` is ever
/// expanded in after load.
///
/// The provider runtime expands `${VAR}` in a base URL when it builds a
/// request. By then the config is one merged document, and a URL can have
/// reached it through any key of any source. Rather than list the keys a
/// project's config might carry a URL in, the runtime asks where the
/// *value* came from: it is the host's when the host's own config
/// (`ConfigLoader::load_trusted`) or the model catalog holds that exact
/// string. A value from anywhere else is never expanded at runtime,
/// whatever key carried it — at worst it is a string the host wrote too,
/// and then it expands to what the host's would.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostStrings(std::collections::HashSet<String>);

impl HostStrings {
    /// Every string holding a `${` anywhere in `documents` (the host's
    /// config, the catalog).
    pub fn collect<'a>(documents: impl IntoIterator<Item = &'a serde_json::Value>) -> HostStrings {
        fn walk(value: &serde_json::Value, out: &mut std::collections::HashSet<String>) {
            match value {
                serde_json::Value::String(text) if text.contains("${") => {
                    out.insert(text.clone());
                }
                serde_json::Value::Array(items) => items.iter().for_each(|item| walk(item, out)),
                serde_json::Value::Object(fields) => {
                    fields.values().for_each(|item| walk(item, out))
                }
                _ => {}
            }
        }
        let mut out = std::collections::HashSet::new();
        for document in documents {
            walk(document, &mut out);
        }
        HostStrings(out)
    }

    pub fn contains(&self, text: &str) -> bool {
        self.0.contains(text)
    }

    /// Expand `${VAR}` in `text` if the host wrote it, under `allowed` (the
    /// host's own rule). A string of any other origin is returned as it is:
    /// nothing in it is expanded. So is the host's own when it names a
    /// variable `allowed` refuses.
    pub fn expand(
        &self,
        text: &str,
        allowed: &dyn Fn(&str) -> bool,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> String {
        if !text.contains("${") {
            return text.to_string();
        }
        if !self.contains(text) {
            tracing::warn!(
                "not expanding ${{…}} in {text}: the value does not come from the host config"
            );
            return text.to_string();
        }
        match interpolate_dollar(text, allowed, lookup) {
            Ok(expanded) => expanded,
            Err(name) => {
                tracing::warn!("not expanding {text}: ${{{name}}} is withheld from URLs");
                text.to_string()
            }
        }
    }
}

/// Read a confined `{file:path}`: the path must be relative and is opened
/// beneath `root` (`crate::beneath::open_beneath`), so no `..`, absolute
/// path or symlink leads out — and a path that cannot be resolved is
/// refused, never accepted on how it is spelled. `Err` is why it is
/// refused; a file that is merely absent is `Ok` with the read error.
fn confined_read(
    config_dir: &Path,
    file_path: &str,
    root: Option<&Path>,
) -> Result<(PathBuf, std::io::Result<String>), String> {
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
    // Where the reference points, relative to the root: the config file
    // sits lexically inside it.
    let resolved = resolve_file_path(config_dir, file_path);
    let lexical_root = resolve_file_path(root, ".");
    let relative = resolved
        .strip_prefix(&lexical_root)
        .map_err(|_| outside())?;
    match crate::beneath::read_to_string_beneath(root, relative) {
        Ok(text) => Ok((resolved, Ok(text))),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok((resolved, Err(error))),
        Err(error) if error.kind() == ErrorKind::PermissionDenied => Err(outside()),
        Err(error) => Ok((resolved, Err(error))),
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
    fn dollar_expansion_is_one_pass_and_a_refusal_is_an_error() {
        let lookup = |name: &str| match name {
            "REGION" => Some("eu".to_string()),
            "NESTED" => Some("${SECRET}".to_string()),
            "BRACE" => Some("{SECRET}".to_string()),
            "SECRET" => Some("hunter2".to_string()),
            _ => None,
        };
        let open = |name: &str| name != "SECRET";
        let expand = |text: &str| interpolate_dollar(text, &open, &lookup);
        assert_eq!(
            expand("https://${REGION}.x/${UNSET}").unwrap(),
            "https://eu.x/${UNSET}"
        );
        // Refused: an error naming the variable, not a URL.
        assert_eq!(expand("https://x/${SECRET}"), Err("SECRET".to_string()));
        // A value is not scanned again…
        assert_eq!(
            expand("https://x/${NESTED}").unwrap(),
            "https://x/${SECRET}"
        );
        assert_eq!(
            expand("https://x/$${BRACE}").unwrap(),
            "https://x/${SECRET}"
        );
        // …and the final form leaves nothing for a later pass to expand.
        let last = |text: &str| interpolate_dollar_final(text, &open, &lookup).unwrap();
        for text in [
            "https://x/${NESTED}",
            "https://x/$${BRACE}",
            "https://x/${UNSET}",
        ] {
            let once = last(text);
            assert!(!once.contains("${"), "{once}");
            let again = interpolate_dollar(&once, &|_| true, &lookup).unwrap();
            assert_eq!(again, once);
            assert!(!again.contains("hunter2"));
        }
        assert_eq!(last("https://x/${NESTED}"), "https://x/%24{SECRET}");
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

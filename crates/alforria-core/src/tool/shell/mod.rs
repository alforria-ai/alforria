//! The bash tool — port of `tool/shell.ts` (spec M4.4).
//!
//! Pipeline: parse the command with tree-sitter (bash grammar) or the
//! conservative fallback (PowerShell/cmd), scan it for external directories
//! and permission patterns, ask `external_directory` then `bash`, and run
//! through the run loop with the instance timeout (+100 ms grace, kill
//! escalating to force after 3 s), a `maxBytes * 2` rolling output window, a
//! 30 KB metadata preview, and a spill file once the output busts the
//! truncation limits.

pub mod arity;
pub mod id;
pub mod parse;
pub mod prompt;

pub use id::TOOL_ID;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;

use crate::tool::def::{
    define, Agents, BoxFuture, ExecuteResult, InstanceContext, MetadataInput, ToolCtxRef, ToolDef,
};
use crate::tool::error::ToolError;
use crate::tool::external_directory::contains_path;
use crate::tool::shell::parse::{CommandNode, Part};
use crate::tool::truncate::Truncate;

/// `MAX_METADATA_LENGTH` (shell.ts:27).
const MAX_METADATA_LENGTH: usize = 30_000;

/// Grace period between SIGTERM and SIGKILL when killing the process group
/// (TS `forceKillAfter: "3 seconds"`, shell.ts:551/555).
const FORCE_KILL_AFTER: Duration = Duration::from_secs(3);

const CWD: &[&str] = &[
    "cd",
    "chdir",
    "popd",
    "pushd",
    "push-location",
    "set-location",
];

// Leave PowerShell aliases out for now. Common ones like cat/cp/mv/rm/mkdir
// already hit the entries above, and alias normalization should happen in
// one place later so we do not risk double-prompting.
const FILES: &[&str] = &[
    "cd",
    "chdir",
    "popd",
    "pushd",
    "push-location",
    "set-location",
    "rm",
    "cp",
    "mv",
    "mkdir",
    "touch",
    "chmod",
    "chown",
    "cat",
    "get-content",
    "set-content",
    "add-content",
    "copy-item",
    "move-item",
    "remove-item",
    "new-item",
    "rename-item",
];

const CMD_FILES: &[&str] = &[
    "copy", "del", "dir", "erase", "md", "mkdir", "move", "rd", "ren", "rename", "rmdir", "type",
];

const FLAGS: &[&str] = &["-destination", "-literalpath", "-path"];

const SWITCHES: &[&str] = &[
    "-confirm",
    "-debug",
    "-force",
    "-nonewline",
    "-recurse",
    "-verbose",
    "-whatif",
];

// ---------------------------------------------------------------------------
// Scan helpers (shell.ts:127-255)
// ---------------------------------------------------------------------------

/// `unquote` (shell.ts:127-133): strip a matching quote pair.
fn unquote(text: &str) -> &str {
    if text.len() < 2 {
        return text;
    }
    let first = text.as_bytes()[0] as char;
    let last = text.as_bytes()[text.len() - 1] as char;
    if (first == '"' || first == '\'') && first == last {
        return &text[1..text.len() - 1];
    }
    text
}

/// `home` (shell.ts:135-139): `~` expansion.
fn home(text: &str) -> String {
    let home_dir = dirs::home_dir().unwrap_or_default();
    if text == "~" {
        return home_dir.display().to_string();
    }
    if text.starts_with("~/") || text.starts_with("~\\") {
        return home_dir.join(&text[2..]).display().to_string();
    }
    text.to_string()
}

/// `envValue` (shell.ts:141-145) — non-win32: a plain case-sensitive lookup.
fn env_value(key: &str) -> String {
    std::env::var(key).unwrap_or_default()
}

/// `auto` (shell.ts:147-152).
fn auto(key: &str, cwd: &str, shell: &str) -> String {
    let name = key.to_uppercase();
    match name.as_str() {
        "HOME" => dirs::home_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        "PWD" => cwd.to_string(),
        "PSHOME" => Path::new(shell)
            .parent()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// `expand` (shell.ts:154-160) — PowerShell-style variable expansion.
fn expand(text: &str, cwd: &str, shell: &str) -> String {
    use std::sync::OnceLock;
    static BRACED: OnceLock<regex::Regex> = OnceLock::new();
    static DOLLAR: OnceLock<regex::Regex> = OnceLock::new();
    static AUTO: OnceLock<regex::Regex> = OnceLock::new();
    let braced = BRACED.get_or_init(|| regex::Regex::new(r"(?i)\$\{env:([^}]+)\}").expect("regex"));
    let dollar = DOLLAR
        .get_or_init(|| regex::Regex::new(r"(?i)\$env:([A-Za-z_][A-Za-z0-9_]*)").expect("regex"));
    let auto_re = AUTO.get_or_init(|| {
        // TS: /\$(HOME|PWD|PSHOME)(?=$|[\/])/gi — a lookahead; the Rust
        // regex crate has none, so consume (and re-append) the separator or
        // require end-of-input instead.
        regex::Regex::new(r"(?i)\$(HOME|PWD|PSHOME)(?:([\\/])|$)").expect("regex")
    });

    let text = unquote(text);
    let text = braced.replace_all(text, |caps: &regex::Captures<'_>| env_value(&caps[1]));
    let text = dollar.replace_all(&text, |caps: &regex::Captures<'_>| env_value(&caps[1]));
    let text = auto_re.replace_all(&text, |caps: &regex::Captures<'_>| match caps.get(2) {
        Some(sep) => format!("{}{}", auto(&caps[1], cwd, shell), sep.as_str()),
        None => auto(&caps[1], cwd, shell),
    });
    home(&text)
}

/// `provider` (shell.ts:162-172): filesystem-provider stripping; single-letter
/// drive prefixes pass through.
fn provider(text: &str) -> Option<String> {
    use std::sync::OnceLock;
    static PROVIDER: OnceLock<regex::Regex> = OnceLock::new();
    static PREFIX: OnceLock<regex::Regex> = OnceLock::new();
    let provider_re =
        PROVIDER.get_or_init(|| regex::Regex::new(r"^([A-Za-z]+)::(.*)$").expect("regex"));
    let prefix_re = PREFIX.get_or_init(|| regex::Regex::new(r"^([A-Za-z]+):(.*)$").expect("regex"));

    if let Some(caps) = provider_re.captures(text) {
        if caps[1].to_lowercase() != "filesystem" {
            return None;
        }
        return Some(caps[2].to_string());
    }
    let caps = match prefix_re.captures(text) {
        Some(caps) => caps,
        None => return Some(text.to_string()),
    };
    if caps[1].len() == 1 {
        return Some(text.to_string());
    }
    None
}

/// `dynamic` (shell.ts:174-179): content that cannot be resolved lexically.
fn dynamic(text: &str, ps: bool) -> bool {
    use std::sync::OnceLock;
    static PS_DOLLAR: OnceLock<regex::Regex> = OnceLock::new();
    if text.starts_with('(') || text.starts_with("@(") {
        return true;
    }
    if text.contains("$(") || text.contains("${") || text.contains('`') {
        return true;
    }
    if ps {
        let _ = &PS_DOLLAR;
        // $(?!env:) — a `$` not beginning an `env:` reference (look-around is
        // unsupported by the regex crate; spelled out by hand).
        let lower = text.to_ascii_lowercase();
        lower
            .match_indices('$')
            .any(|(index, _)| !lower[index + 1..].starts_with("env:"))
    } else {
        text.contains('$')
    }
}

/// `prefix` (shell.ts:181-186): cut at the first glob character.
fn glob_prefix(text: &str) -> Option<&str> {
    match text.find(['?', '*', '[']) {
        Some(0) => None,
        Some(index) => Some(&text[..index]),
        // TS: text.slice(0, undefined) — no glob char means the whole text.
        None => Some(text),
    }
}

/// `pathArgs` (shell.ts:188-218).
fn path_args(list: &[Part], ps: bool, cmd: bool) -> Vec<String> {
    if !ps {
        let is_chmod = list.first().map(|p| p.text == "chmod").unwrap_or(false);
        return list
            .iter()
            .skip(1)
            .filter(|item| {
                !(item.text.starts_with('-')
                    || (cmd && item.text.starts_with('/'))
                    || (is_chmod && item.text.starts_with('+')))
            })
            .map(|item| item.text.clone())
            .collect();
    }

    let mut out = Vec::new();
    let mut want = false;
    for item in list.iter().skip(1) {
        if want {
            out.push(item.text.clone());
            want = false;
            continue;
        }
        if item.kind == "command_parameter" {
            let flag = item.text.to_lowercase();
            if SWITCHES.contains(&flag.as_str()) {
                continue;
            }
            want = FLAGS.contains(&flag.as_str());
            continue;
        }
        out.push(item.text.clone());
    }
    out
}

/// `preview` (shell.ts:220-223) — last `MAX_METADATA_LENGTH` bytes.
fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

fn preview(text: &str) -> String {
    // TS `text.slice(-MAX_METADATA_LENGTH)` truncates UTF-16 code units,
    // not bytes — a close fit requires an encode_utf16 walk.
    if utf16_len(text) <= MAX_METADATA_LENGTH {
        return text.to_string();
    }
    let skip = utf16_len(text) - MAX_METADATA_LENGTH;
    let mut seen = 0usize;
    let mut start = 0usize;
    for (at, ch) in text.char_indices() {
        if seen >= skip {
            start = at;
            break;
        }
        seen += ch.len_utf16();
        start = at + ch.len_utf8();
    }
    format!("...\n\n{}", &text[start..])
}

/// `tail` (shell.ts:225-255): keep the last lines/bytes, avoiding a UTF-8
/// continuation-byte start when the oldest surviving line alone busts the
/// byte budget.
fn tail(text: &str, max_lines: usize, max_bytes: usize) -> (String, bool) {
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.len() <= max_lines && text.len() <= max_bytes {
        return (text.to_string(), false);
    }

    let mut out: Vec<&str> = Vec::new();
    let mut bytes = 0usize;
    let mut i = lines.len();
    while i > 0 && out.len() < max_lines {
        i -= 1;
        let size = lines[i].len() + usize::from(!out.is_empty());
        if bytes + size > max_bytes {
            if out.is_empty() {
                let buf = lines[i].as_bytes();
                let mut start = buf.len().saturating_sub(max_bytes);
                while start < buf.len() && (buf[start] & 0xc0) == 0x80 {
                    start += 1;
                }
                let slice = &buf[start..];
                out.insert(0, std::str::from_utf8(slice).unwrap_or(""));
            }
            break;
        }
        out.insert(0, lines[i]);
        bytes += size;
    }
    (out.join("\n"), true)
}

/// Lexical `path.normalize`-style resolution: collapse `.`/`..` without
/// touching the filesystem (`path.resolve` on the already-absolute join).
fn normalize(path: &Path) -> PathBuf {
    use std::ffi::OsString;
    use std::path::Component;

    let mut out: Vec<OsString> = Vec::new();
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir | Component::Prefix(_) => {
                result = PathBuf::from(component.as_os_str());
                out.clear();
            }
            Component::CurDir => {}
            // `..` above the root clamps to the root (path.resolve).
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => out.push(part.to_os_string()),
        }
    }
    for part in out {
        result.push(part);
    }
    if result.as_os_str().is_empty() {
        result = PathBuf::from(".");
    }
    result
}

fn resolve_path(root: &Path, text: &str) -> PathBuf {
    let path = Path::new(text);
    if path.is_absolute() {
        normalize(path)
    } else {
        normalize(&root.join(path))
    }
}

fn dirname(path: &Path) -> PathBuf {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// `argPath` (shell.ts:369-376) — resolve a command argument to a path;
/// `None` when it is empty, dynamic or glob-only.
fn arg_path(arg: &str, cwd: &Path, ps: bool, shell: &str) -> Option<PathBuf> {
    let text = if ps {
        expand(arg, &cwd.display().to_string(), shell)
    } else {
        home(unquote(arg))
    };
    let file = if text.is_empty() {
        None
    } else {
        glob_prefix(&text)
    };
    let file = file?;
    if dynamic(file, ps) {
        return None;
    }
    let next = if ps {
        provider(file)?
    } else {
        file.to_string()
    };
    Some(resolve_path(cwd, &next))
}

// ---------------------------------------------------------------------------
// Scan (shell.ts:378-414)
// ---------------------------------------------------------------------------

/// `Scan` (shell.ts:73-77) — insertion-ordered sets.
#[derive(Debug, Default, Clone)]
pub struct Scan {
    pub dirs: Vec<String>,
    pub patterns: Vec<String>,
    pub always: Vec<String>,
}

impl Scan {
    fn push_dir(&mut self, dir: String) {
        if !self.dirs.contains(&dir) {
            self.dirs.push(dir);
        }
    }

    fn push_pattern(&mut self, pattern: String) {
        if !self.patterns.contains(&pattern) {
            self.patterns.push(pattern);
        }
    }

    fn push_always(&mut self, always: String) {
        if !self.always.contains(&always) {
            self.always.push(always);
        }
    }
}

/// `Shell.name` (core/shell.ts): lowercase basename.
fn shell_name(shell: &str) -> String {
    Path::new(shell)
        .file_name()
        .map(|name| name.to_string_lossy().to_lowercase())
        .unwrap_or_else(|| shell.to_lowercase())
}

/// `Shell.ps` (core/shell.ts).
fn is_ps(shell: &str) -> bool {
    matches!(shell_name(shell).as_str(), "powershell" | "pwsh")
}

async fn is_dir(path: &Path) -> bool {
    tokio::fs::metadata(path)
        .await
        .map(|meta| meta.is_dir())
        .unwrap_or(false)
}

/// `collect` (shell.ts:378-414): scan every command node for external
/// directories and the `bash` permission patterns.
pub async fn collect(
    nodes: &[CommandNode],
    cwd: &Path,
    ps: bool,
    shell: &str,
    instance: &InstanceContext,
) -> Scan {
    let mut scan = Scan::default();
    let shell_kind = id::to_kind(&shell_name(shell));

    for node in nodes {
        let tokens: Vec<&str> = node.parts.iter().map(|p| p.text.as_str()).collect();
        let cmd = tokens.first().map(|t| {
            if ps || shell_kind == "cmd" {
                t.to_lowercase()
            } else {
                t.to_string()
            }
        });

        if let Some(cmd) = &cmd {
            if FILES.contains(&cmd.as_str())
                || (shell_kind == "cmd" && CMD_FILES.contains(&cmd.as_str()))
            {
                for arg in path_args(&node.parts, ps, shell_kind == "cmd") {
                    let Some(resolved) = arg_path(&arg, cwd, ps, shell) else {
                        continue;
                    };
                    if contains_path(&resolved, instance) {
                        continue;
                    }
                    let dir = if is_dir(&resolved).await {
                        resolved
                    } else {
                        dirname(&resolved)
                    };
                    scan.push_dir(dir.display().to_string());
                }
            }
        }

        let is_cwd = cmd.as_deref().is_some_and(|cmd| CWD.contains(&cmd));
        if !tokens.is_empty() && !is_cwd {
            scan.push_pattern(node.source.clone());
            let prefix = arity::prefix(&tokens).join(" ");
            scan.push_always(format!("{prefix} *"));
        }
    }

    scan
}

/// `ask` (shell.ts:263-291): `external_directory` first (one ask with a
/// `dir/*` glob per external dir), then the `bash` permission.
async fn ask(ctx: &ToolCtxRef<'_>, scan: &Scan, command: &str) -> Result<(), ToolError> {
    if !scan.dirs.is_empty() {
        let directories = scan.dirs.clone();
        let globs = directories
            .iter()
            .map(|dir| {
                Path::new(dir)
                    .join("*")
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect::<Vec<_>>();
        ctx.ask
            .ask(crate::tool::def::AskRequest {
                permission: "external_directory".to_string(),
                patterns: globs.clone(),
                always: globs.clone(),
                metadata: json!({
                    "command": command,
                    "directories": directories,
                    "patterns": globs,
                }),
            })
            .await?;
    }

    if scan.patterns.is_empty() {
        return Ok(());
    }
    ctx.ask
        .ask(crate::tool::def::AskRequest {
            permission: TOOL_ID.to_string(),
            patterns: scan.patterns.clone(),
            always: scan.always.clone(),
            metadata: json!({
                "command": command,
            }),
        })
        .await
}

// ---------------------------------------------------------------------------
// Spawner seam (TS ChildProcessSpawner)
// ---------------------------------------------------------------------------

/// A spawn request (TS `cmd(input…)` payload, shell.ts:293-310).
#[derive(Debug, Clone)]
pub struct ShellSpawn {
    pub shell: String,
    pub command: String,
    pub cwd: PathBuf,
}

pub type KillFn = Arc<dyn Fn(Duration) -> BoxFuture<'static, ()> + Send + Sync>;

/// A spawned process as the run loop consumes it: merged stdout+stderr
/// chunks, the exit code future (`None` when killed by a signal), and a
/// kill handle that escalates to a force kill after the given delay.
pub struct SpawnedProcess {
    pub chunks: tokio::sync::mpsc::Receiver<String>,
    pub exit: BoxFuture<'static, Option<i64>>,
    pub kill: KillFn,
}

pub trait ShellSpawner: Send + Sync {
    fn spawn(&self, request: ShellSpawn) -> BoxFuture<'static, Result<SpawnedProcess, ToolError>>;
}

/// Production spawner: `shell -c command`, stdin ignored, detached process
/// group on Unix (`detached: platform !== "win32"`), child environment =
/// process environment (the plugin `shell.env` seam is a no-op in M4).
#[derive(Debug, Default)]
pub struct TokioSpawner;

impl ShellSpawner for TokioSpawner {
    fn spawn(&self, request: ShellSpawn) -> BoxFuture<'static, Result<SpawnedProcess, ToolError>> {
        Box::pin(async move {
            let mut cmd = tokio::process::Command::new(&request.shell);
            cmd.arg("-c").arg(&request.command);
            cmd.current_dir(&request.cwd);
            cmd.stdin(std::process::Stdio::null());
            cmd.stdout(std::process::Stdio::piped());
            cmd.stderr(std::process::Stdio::piped());
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                cmd.as_std_mut().process_group(0);
            }
            let mut child = cmd
                .spawn()
                .map_err(|err| ToolError::Failed(format!("Failed to spawn command: {err}")))?;
            let pid = child.id().unwrap_or(0);

            let stdout = child.stdout.take();
            let stderr = child.stderr.take();
            let (chunk_tx, chunk_rx) = tokio::sync::mpsc::channel::<String>(64);
            fn spawn_reader<R>(stream: R, chunk_tx: tokio::sync::mpsc::Sender<String>)
            where
                R: tokio::io::AsyncRead + Unpin + Send + 'static,
            {
                tokio::spawn(async move {
                    use tokio::io::AsyncReadExt;
                    let mut stream = stream;
                    let mut buf = [0u8; 8192];
                    loop {
                        match stream.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                                if chunk_tx.send(text).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
            if let Some(stream) = stdout {
                spawn_reader(stream, chunk_tx.clone());
            }
            if let Some(stream) = stderr {
                spawn_reader(stream, chunk_tx.clone());
            }
            drop(chunk_tx);

            let exited = Arc::new(AtomicBool::new(false));
            let (exit_tx, exit_rx) = tokio::sync::oneshot::channel::<Option<i64>>();
            let exited_flag = Arc::clone(&exited);
            tokio::spawn(async move {
                let code = match child.wait().await {
                    Ok(status) => status.code().map(|code| code as i64),
                    Err(_) => None,
                };
                exited_flag.store(true, AtomicOrdering::SeqCst);
                let _ = exit_tx.send(code);
            });

            let kill: KillFn = {
                let exited = Arc::clone(&exited);
                Arc::new(move |force_after: Duration| {
                    let exited = Arc::clone(&exited);
                    Box::pin(async move {
                        let pgid = pid as libc::pid_t;
                        unsafe { libc::kill(-pgid, libc::SIGTERM) };
                        let deadline = tokio::time::Instant::now() + force_after;
                        while tokio::time::Instant::now() < deadline {
                            if exited.load(AtomicOrdering::SeqCst) {
                                return;
                            }
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                        if !exited.load(AtomicOrdering::SeqCst) {
                            unsafe { libc::kill(-pgid, libc::SIGKILL) };
                        }
                    }) as BoxFuture<'static, ()>
                })
            };

            Ok(SpawnedProcess {
                chunks: chunk_rx,
                exit: Box::pin(async move { exit_rx.await.unwrap_or(None) }),
                kill,
            })
        })
    }
}

// ---------------------------------------------------------------------------
// Run loop (shell.ts:428-644)
// ---------------------------------------------------------------------------

/// The bash tool parameters (TS `Parameters`, shell/prompt.ts:15-23).
#[derive(Debug, Clone, Deserialize)]
pub struct Parameters {
    pub command: String,
    pub timeout: Option<i64>,
    pub workdir: Option<String>,
}

#[derive(Debug)]
struct RunInput {
    shell: String,
    command: String,
    cwd: PathBuf,
    timeout: u64,
}

fn metadata_output(output: &str) -> MetadataInput {
    MetadataInput {
        title: None,
        metadata: Some(json!({ "output": output })),
    }
}

/// The run loop (shell.ts:428-595): rolling output window, per-chunk
/// metadata, spill file at the truncation limit, and the
/// exit/abort/timeout race.
async fn run(
    truncate: &Arc<dyn Truncate>,
    spawner: &Arc<dyn ShellSpawner>,
    input: RunInput,
    ctx: &ToolCtxRef<'_>,
) -> Result<ExecuteResult, ToolError> {
    let (max_lines, max_bytes) = truncate.limits().await;
    let keep = max_bytes * 2;

    let mut list: VecDeque<String> = VecDeque::new();
    let mut used = 0usize;
    let mut last = String::new();
    let mut full = String::new();
    let mut file: Option<PathBuf> = None;
    let mut sink: Option<tokio::fs::File> = None;
    let mut cut = false;
    let mut expired = false;
    let mut aborted = false;

    ctx.metadata.metadata(metadata_output("")).await?;

    let spawned = spawner
        .spawn(ShellSpawn {
            shell: input.shell.clone(),
            command: input.command.clone(),
            cwd: input.cwd.clone(),
        })
        .await?;
    let SpawnedProcess {
        mut chunks,
        mut exit,
        kill,
    } = spawned;
    let mut chunks_open = true;

    let timeout = tokio::time::sleep(Duration::from_millis(input.timeout + 100));
    tokio::pin!(timeout);

    let code = loop {
        tokio::select! {
            chunk = async { chunks.recv().await }, if chunks_open => {
                match chunk {
                    Some(chunk) => {
                        used += chunk.len();
                        list.push_back(chunk.clone());
                        while used > keep && list.len() > 1 {
                            match list.pop_front() {
                                Some(item) => {
                                    used -= item.len();
                                    cut = true;
                                }
                                None => break,
                            }
                        }

                        let combined = format!("{last}{chunk}");
                        last = preview(&combined);

                        if file.is_some() {
                            if let Some(sink) = sink.as_mut() {
                                use tokio::io::AsyncWriteExt;
                                sink.write_all(chunk.as_bytes()).await.map_err(|err| {
                                    ToolError::Failed(format!("Failed to write output file: {err}"))
                                })?;
                            }
                        } else {
                            full.push_str(&chunk);
                            if full.len() > max_bytes {
                                let next = truncate.write(&full).await;
                                file = Some(next.clone());
                                cut = true;
                                let opened = tokio::fs::OpenOptions::new()
                                    .append(true)
                                    .open(&next)
                                    .await
                                    .map_err(|err| {
                                        ToolError::Failed(format!("Failed to open output file: {err}"))
                                    })?;
                                sink = Some(opened);
                                full.clear();
                                ctx.metadata.metadata(metadata_output(&last)).await?;
                            }
                        }
                        ctx.metadata.metadata(metadata_output(&last)).await?;
                    }
                    None => chunks_open = false,
                }
            }
            code = &mut exit => {
                // Drain until the channel closes — the exit status can win
                // the race while the reader tasks still hold a final read,
                // so a one-shot try_recv drain would drop trailing chunks
                // (TS's forked stream consumer runs until the merged
                // stream ends).
                while let Some(chunk) = chunks.recv().await {
                            used += chunk.len();
                            list.push_back(chunk.clone());
                            while used > keep && list.len() > 1 {
                                match list.pop_front() {
                                    Some(item) => {
                                        used -= item.len();
                                        cut = true;
                                    }
                                    None => break,
                                }
                            }
                            let combined = format!("{last}{chunk}");
                            last = preview(&combined);
                            if file.is_some() {
                                if let Some(sink) = sink.as_mut() {
                                    use tokio::io::AsyncWriteExt;
                                    sink.write_all(chunk.as_bytes()).await.map_err(|err| {
                                        ToolError::Failed(format!("Failed to write output file: {err}"))
                                    })?;
                                }
                            } else {
                                full.push_str(&chunk);
                                if full.len() > max_bytes {
                                    let next = truncate.write(&full).await;
                                    file = Some(next.clone());
                                    cut = true;
                                    let opened = tokio::fs::OpenOptions::new()
                                        .append(true)
                                        .open(&next)
                                        .await
                                        .map_err(|err| {
                                            ToolError::Failed(format!("Failed to open output file: {err}"))
                                        })?;
                                    sink = Some(opened);
                                    full.clear();
                                }
                            }
                        }
                break code;
            }
            _ = &mut timeout => {
                expired = true;
                kill(FORCE_KILL_AFTER).await;
                break None;
            }
            _ = ctx.abort.cancelled() => {
                aborted = true;
                kill(FORCE_KILL_AFTER).await;
                break None;
            }
        }
    };
    // Scoped exit of the chunk sink mirrors closeSink (shell.ts:450-473).
    drop(sink);

    let mut meta: Vec<String> = Vec::new();
    if expired {
        meta.push(format!(
            "shell tool terminated command after exceeding timeout {timeout} ms. If this command is expected to take longer and is not waiting for interactive input, retry with a larger timeout value in milliseconds.",
            timeout = input.timeout,
        ));
    }
    if aborted {
        meta.push("User aborted the command".to_string());
    }
    let raw: String = list.iter().map(|item| item.as_str()).collect();
    let (mut output, end_cut) = tail(&raw, max_lines, max_bytes);
    if end_cut {
        cut = true;
    }
    if file.is_none() && end_cut {
        file = Some(truncate.write(&raw).await);
    }

    if output.is_empty() {
        output = "(no output)".to_string();
    }

    if cut && file.is_some() {
        let file = file.as_deref().expect("checked").display();
        output = format!("...output truncated...\n\nFull output saved to: {file}\n\n{output}");
    }

    if !meta.is_empty() {
        output += &format!(
            "\n\n<shell_metadata>\n{}\n</shell_metadata>",
            meta.join("\n")
        );
    }

    let metadata_output = if last.is_empty() {
        preview(&output)
    } else {
        last
    };
    let mut metadata = json!({
        "output": metadata_output,
        "exit": code,
        "truncated": cut,
    });
    if cut {
        if let Some(file) = &file {
            metadata["outputPath"] = json!(file.display().to_string());
        }
    }

    Ok(ExecuteResult {
        title: input.command.clone(),
        metadata,
        output,
        attachments: None,
    })
}

// ---------------------------------------------------------------------------
// Tool (shell.ts:338-645)
// ---------------------------------------------------------------------------

/// `Global.Path.tmp` — `path.join(os.tmpdir(), app)` (core/global.ts:15).
fn tmp_dir() -> PathBuf {
    std::env::temp_dir().join("opencode")
}

/// `flags.bashDefaultTimeoutMs ?? 2 * 60 * 1000` (shell.ts:347).
pub fn default_timeout_ms() -> u64 {
    2 * 60 * 1000
}

/// The bash tool: rendered prompt + run pipeline against the M4.1 seam.
pub struct ShellTool {
    /// The (already acceptable) shell, e.g. `"bash"` or `"/bin/zsh"`.
    pub shell: String,
    pub default_timeout_ms: u64,
    pub truncate: Arc<dyn Truncate>,
    pub spawner: Arc<dyn ShellSpawner>,
}

impl ShellTool {
    pub fn new(
        shell: impl Into<String>,
        default_timeout_ms: u64,
        truncate: Arc<dyn Truncate>,
        spawner: Arc<dyn ShellSpawner>,
    ) -> Self {
        ShellTool {
            shell: shell.into(),
            default_timeout_ms,
            truncate,
            spawner,
        }
    }

    /// `ShellPrompt.render` (shell.ts:597-604) — rendered once per instance.
    pub async fn render(&self) -> anyhow::Result<prompt::RenderedPrompt> {
        let (max_lines, max_bytes) = self.truncate.limits().await;
        prompt::render(
            &shell_name(&self.shell),
            std::env::consts::OS,
            prompt::Limits {
                max_lines,
                max_bytes,
            },
            self.default_timeout_ms,
            &tmp_dir(),
        )
    }

    /// Build the registered `bash` tool definition.
    pub async fn def(&self, agents: Arc<dyn Agents>) -> ToolDef {
        let rendered = self.render().await.expect("complete prompt values");
        let shell = self.shell.clone();
        let default_timeout_ms = self.default_timeout_ms;
        let truncate = Arc::clone(&self.truncate);
        let spawner = Arc::clone(&self.spawner);
        define::<Parameters, _>(
            TOOL_ID,
            rendered.description,
            rendered.parameters,
            None,
            Arc::clone(&self.truncate),
            agents,
            move |params: Parameters, ctx: ToolCtxRef<'_>| {
                let (shell, default_timeout_ms, truncate, spawner) = (
                    shell.clone(),
                    default_timeout_ms,
                    Arc::clone(&truncate),
                    Arc::clone(&spawner),
                );
                Box::pin(async move {
                    let name = shell_name(&shell);
                    let ps = is_ps(&shell);
                    let cwd = match &params.workdir {
                        Some(workdir) => resolve_path(&ctx.instance.directory, workdir),
                        None => ctx.instance.directory.clone(),
                    };
                    if let Some(timeout) = params.timeout {
                        if timeout < 0 {
                            return Err(ToolError::Failed(format!(
                                "Invalid timeout value: {timeout}. Timeout must be a positive number."
                            )));
                        }
                    }
                    let timeout =
                        u64::try_from(params.timeout.unwrap_or(default_timeout_ms as i64))
                            .unwrap_or(default_timeout_ms);

                    let parser = parse::parser_for(&name);
                    let nodes = parser
                        .parse(&params.command)
                        .map_err(|e| ToolError::Failed(e.to_string()))?;
                    let mut scan = collect(&nodes, &cwd, ps, &shell, ctx.instance).await;
                    if !contains_path(&cwd, ctx.instance) {
                        scan.push_dir(cwd.display().to_string());
                    }
                    ask(&ctx, &scan, &params.command).await?;

                    run(
                        &truncate,
                        &spawner,
                        RunInput {
                            shell: shell.clone(),
                            command: params.command.clone(),
                            cwd,
                            timeout,
                        },
                        &ctx,
                    )
                    .await
                }) as BoxFuture<'_, Result<ExecuteResult, ToolError>>
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::tool::def::{Ask, AskRequest, Extra, MetadataSink};
    use crate::tool::permission::Ruleset;
    use crate::tool::truncate::TruncateService;
    use crate::tool::truncate::MAX_BYTES as TRUNC_MAX_BYTES;
    use crate::tool::truncate::MAX_LINES as TRUNC_MAX_LINES;

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

    struct FixedAgents;

    impl Agents for FixedAgents {
        fn get<'a>(
            &'a self,
            _agent: &'a str,
        ) -> BoxFuture<'a, Result<crate::tool::def::AgentInfo, ToolError>> {
            Box::pin(async move {
                Ok(crate::tool::def::AgentInfo {
                    name: "build".to_string(),
                    description: None,
                    mode: crate::tool::def::AgentMode::Primary,
                    permission: Ruleset::new(),
                })
            })
        }

        fn list<'a>(&'a self) -> BoxFuture<'a, Vec<crate::tool::def::AgentInfo>> {
            Box::pin(async move { Vec::new() })
        }
    }

    fn temp(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("opencode-shell-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[allow(dead_code)]
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

    // ------------------------------------------------------------------
    // Helper helpers
    // ------------------------------------------------------------------

    #[test]
    fn unquote_strips_matching_pair() {
        assert_eq!(unquote("\"a b\""), "a b");
        assert_eq!(unquote("'a b'"), "a b");
        assert_eq!(unquote("\"a'"), "\"a'");
        assert_eq!(unquote("\""), "\"");
        assert_eq!(unquote(""), "");
    }

    #[test]
    fn glob_prefix_cuts_at_first_glob_char() {
        assert_eq!(glob_prefix("/tmp/fo*"), Some("/tmp/fo"));
        assert_eq!(glob_prefix("/tmp"), Some("/tmp"));
        assert_eq!(glob_prefix("*foo"), None);
        assert_eq!(glob_prefix("a[b"), Some("a"));
    }

    #[test]
    fn auto_var_requires_boundary() {
        // TS lookahead: $HOMEWORK must NOT expand HOME.
        let home = expand("$HOMEWORK", "/tmp", "bash");
        assert_eq!(home, "$HOMEWORK");
        // $HOME alone (end of input) and $HOME/ both expand.
        assert_eq!(
            expand("$HOME", "/tmp", "bash"),
            std::env::var("HOME").unwrap_or_default()
        );
        assert!(expand("$HOME/", "/tmp", "bash").ends_with('/'));
    }

    #[test]
    fn dynamic_detection() {
        assert!(dynamic("(Get-Date)", false));
        assert!(dynamic("@(Get-Date)", false));
        assert!(dynamic("$(x)", false));
        assert!(dynamic("${x}", false));
        assert!(dynamic("`x", false));
        assert!(dynamic("$x", false));
        assert!(!dynamic("plain", false));
        // PS: `$` without `env:` is dynamic, `$env:VAR` is not.
        assert!(!dynamic("$env:VAR", true));
        assert!(dynamic("$var", true));
    }

    #[test]
    fn provider_strips_filesystem_provider() {
        assert_eq!(provider("filesystem::/tmp"), Some("/tmp".to_string()));
        assert_eq!(provider("registry::/tmp"), None);
        assert_eq!(provider("c:/tmp"), Some("c:/tmp".to_string()));
        assert_eq!(provider("ab:/tmp"), None);
        assert_eq!(provider("/tmp"), Some("/tmp".to_string()));
    }

    #[test]
    fn path_args_filters_flags() {
        let parts = |texts: &[&str]| {
            texts
                .iter()
                .map(|t| Part {
                    kind: "word".to_string(),
                    text: t.to_string(),
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            path_args(&parts(&["rm", "-rf", "/tmp"]), false, false),
            vec!["/tmp".to_string()]
        );
        assert_eq!(
            path_args(&parts(&["chmod", "+x", "file"]), false, false),
            vec!["file".to_string()]
        );
        assert_eq!(
            path_args(&parts(&["del", "/x", "a"]), false, true),
            vec!["a".to_string()]
        );
    }

    #[test]
    fn preview_caps_to_last_bytes() {
        let text = "a".repeat(31_000);
        let out = preview(&text);
        assert!(out.starts_with("...\n\n"));
        assert_eq!(out.len(), MAX_METADATA_LENGTH + 5);
    }

    #[test]
    fn tail_utf8_boundary() {
        // "é" is 2 bytes; force the byte budget to split it.
        let text = "éééé";
        let (out, cut) = tail(text, 100, 4);
        assert!(cut);
        // 4 bytes from the end lands mid-"é": the boundary scan must skip
        // the continuation byte and keep valid UTF-8.
        assert_eq!(out, "éé");
    }

    #[test]
    fn tail_keeps_last_lines() {
        let text = "a\nb\nc\nd";
        assert_eq!(tail(text, 10, 100), ("a\nb\nc\nd".to_string(), false));
        let (out, cut) = tail(text, 2, 100);
        assert!(cut);
        assert_eq!(out, "c\nd");
    }

    // ------------------------------------------------------------------
    // collect
    // ------------------------------------------------------------------

    fn scan_sync(command: &str, directory: &Path, worktree: &Path) -> Scan {
        let parser = parse::parser_for("bash");
        let nodes = parser.parse(command).expect("parse");
        let cwd = directory.to_path_buf();
        let instance = InstanceContext {
            directory: directory.to_path_buf(),
            worktree: worktree.to_path_buf(),
        };
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(collect(&nodes, &cwd, false, "bash", &instance))
        })
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn collect_cd_only_command_has_no_patterns() {
        let dir = temp("collect-cd");
        let scan = scan_sync("cd /tmp", &dir, &dir);
        assert!(scan.patterns.is_empty(), "{scan:?}");
        assert!(scan.always.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn collect_pattern_and_arity_always() {
        let dir = temp("collect-pattern");
        let scan = scan_sync("git commit -m \"a b c\"", &dir, &dir);
        assert_eq!(scan.patterns, vec!["git commit -m \"a b c\"".to_string()]);
        assert_eq!(scan.always, vec!["git commit *".to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn collect_files_command_yields_external_dirs() {
        let dir = temp("collect-files");
        let scan = scan_sync("rm -rf /somewhere/else", &dir, &dir);
        assert!(scan.dirs.contains(&"/somewhere".to_string()), "{scan:?}");
        assert_eq!(scan.patterns, vec!["rm -rf /somewhere/else".to_string()]);
        assert_eq!(scan.always, vec!["rm *".to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn collect_files_command_inside_instance_skips_dir() {
        let dir = temp("collect-inside");
        std::fs::write(dir.join("file.txt"), "x").expect("write");
        let scan = scan_sync("rm file.txt", &dir, &dir);
        assert!(scan.dirs.is_empty(), "{scan:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn collect_glob_prefix_cuts_before_scan() {
        let dir = temp("collect-glob");
        let scan = scan_sync("rm -rf /tmp/fo*", &dir, &dir);
        assert_eq!(scan.dirs, vec!["/tmp".to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn collect_dynamic_args_skipped() {
        let dir = temp("collect-dynamic");
        let scan = scan_sync("rm -rf $(echo /tmp)", &dir, &dir);
        assert!(scan.dirs.is_empty(), "{scan:?}");
        // The subshell's `echo /tmp` is also a `command` descendant
        // (commands() = descendantsOfType("command")), so both sources land in
        // patterns; only dirs skip dynamic args.
        assert_eq!(
            scan.patterns,
            vec!["rm -rf $(echo /tmp)".to_string(), "echo /tmp".to_string(),]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn collect_redirect_source_includes_redirect() {
        let dir = temp("collect-redirect");
        let parser = parse::parser_for("bash");
        let nodes = parser.parse("echo hi > /tmp/out.txt").expect("parse");
        let instance = InstanceContext {
            directory: dir.clone(),
            worktree: dir.clone(),
        };
        let cwd = dir.clone();
        let scan = futures_executor_collect(&nodes, &cwd, &instance);
        assert_eq!(scan.patterns, vec!["echo hi > /tmp/out.txt".to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn futures_executor_collect(
        nodes: &[CommandNode],
        cwd: &Path,
        instance: &InstanceContext,
    ) -> Scan {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(collect(nodes, cwd, false, "bash", instance))
        })
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn collect_subshell_commands_scanned() {
        let dir = temp("collect-subshell");
        let scan = scan_sync("(cd /tmp && ls)", &dir, &dir);
        assert_eq!(scan.patterns, vec!["ls".to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn collect_quoted_path_yields_dir() {
        let dir = temp("collect-quoted");
        let scan = scan_sync("rm \"/somewhere/else\"", &dir, &dir);
        assert_eq!(scan.dirs, vec!["/somewhere".to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn normalize_resolves_dotdot() {
        assert_eq!(normalize(Path::new("/a/b/../c")), PathBuf::from("/a/c"));
        assert_eq!(normalize(Path::new("/a/../..")), PathBuf::from("/"));
        assert_eq!(
            resolve_path(Path::new("/base"), "sub/./x"),
            PathBuf::from("/base/sub/x")
        );
        assert_eq!(
            resolve_path(Path::new("/base"), "/abs/path"),
            PathBuf::from("/abs/path")
        );
    }

    // ------------------------------------------------------------------
    // Run loop — fake spawner
    // ------------------------------------------------------------------

    /// Scripted spawner: emits `chunks`, then either exits with `exit` or
    /// never exits; `kill` just marks the kill.
    struct FakeSpawner {
        chunks: Vec<String>,
        exit: Option<Option<i64>>,
        killed: Arc<Mutex<usize>>,
    }

    struct SpawnerConfig(FakeSpawner);

    impl ShellSpawner for SpawnerConfig {
        fn spawn(
            &self,
            _request: ShellSpawn,
        ) -> BoxFuture<'static, Result<SpawnedProcess, ToolError>> {
            let config = &self.0;
            let chunks = config.chunks.clone();
            let exit = config.exit;
            let killed = Arc::clone(&config.killed);
            Box::pin(async move {
                // A real process's output streams before its exit status:
                // resolve the exit future only once every chunk was drained.
                let (chunk_tx, chunk_rx) = tokio::sync::mpsc::channel::<String>(64);
                let (exit_tx, exit_rx) = tokio::sync::oneshot::channel::<Option<i64>>();
                let _sender = tokio::spawn(async move {
                    for chunk in chunks {
                        if chunk_tx.send(chunk).await.is_err() {
                            return;
                        }
                    }
                    drop(chunk_tx);
                    // exit == None models a live process: the exit future
                    // stays pending until the caller is killed.
                    if let Some(code) = exit {
                        let _ = exit_tx.send(code);
                    }
                });
                let exit_fut: BoxFuture<'static, Option<i64>> = Box::pin(async move {
                    match exit_rx.await {
                        Ok(code) => code,
                        Err(_) => std::future::pending().await,
                    }
                });
                let killed = Arc::clone(&killed);
                let kill: KillFn = Arc::new(move |_force_after| {
                    let killed = Arc::clone(&killed);
                    Box::pin(async move {
                        *killed.lock().unwrap() += 1;
                    }) as BoxFuture<'static, ()>
                });
                Ok(SpawnedProcess {
                    chunks: chunk_rx,
                    exit: exit_fut,
                    kill,
                })
            })
        }
    }

    fn shell_tool(spawner: Arc<dyn ShellSpawner>, max_bytes: usize, dir: &Path) -> ShellTool {
        ShellTool::new(
            "bash",
            120_000,
            Arc::new(TruncateService::new(
                dir.to_path_buf(),
                TRUNC_MAX_LINES,
                max_bytes,
            )),
            spawner,
        )
    }

    async fn execute(
        tool: &ShellTool,
        args: serde_json::Value,
    ) -> (ExecuteResult, RecordingAsk, InstanceContext, Extra) {
        let ask = RecordingAsk::new();
        let instance = InstanceContext {
            directory: std::env::temp_dir(),
            worktree: Path::new("/").to_path_buf(),
        };
        let extra = Extra::default();
        let def = tool.def(Arc::new(FixedAgents)).await;
        let ctx = ToolCtxRef {
            session_id: "ses_1",
            message_id: "msg_1",
            agent: "build",
            call_id: None,
            abort: tokio_util::sync::CancellationToken::new(),
            messages: &[],
            extra: &extra,
            instance: &instance,
            ask: &ask,
            metadata: &ask,
        };
        let result = (def.execute)(args, ctx).await.expect("execute succeeds");
        (result, ask, instance, extra)
    }

    #[tokio::test]
    async fn run_echo_and_exit_code() {
        let spawner: Arc<dyn ShellSpawner> = Arc::new(SpawnerConfig(FakeSpawner {
            chunks: vec!["hello\n".to_string()],
            exit: Some(Some(0)),
            killed: Arc::new(Mutex::new(0)),
        }));
        let dir = temp("run-echo");
        let tool = shell_tool(spawner, TRUNC_MAX_BYTES, &dir);
        let (result, ask, _, _) = execute(&tool, json!({ "command": "echo hello" })).await;
        assert_eq!(result.title, "echo hello");
        assert_eq!(result.output, "hello\n");
        assert_eq!(result.metadata["exit"], json!(0));
        assert_eq!(result.metadata["truncated"], json!(false));
        assert_eq!(result.metadata["output"], json!("hello\n"));
        // echo is not a cd command: exactly one bash ask.
        let requests = ask.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].permission, "bash");
        assert_eq!(requests[0].patterns, vec!["echo hello".to_string()]);
        assert_eq!(requests[0].always, vec!["echo *".to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn run_no_output() {
        let spawner: Arc<dyn ShellSpawner> = Arc::new(SpawnerConfig(FakeSpawner {
            chunks: vec![],
            exit: Some(Some(0)),
            killed: Arc::new(Mutex::new(0)),
        }));
        let dir = temp("run-empty");
        let tool = shell_tool(spawner, TRUNC_MAX_BYTES, &dir);
        let (result, _, _, _) = execute(&tool, json!({ "command": "true" })).await;
        assert_eq!(result.output, "(no output)");
        assert_eq!(result.metadata["output"], json!("(no output)"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn run_exit_code_propagated() {
        let spawner: Arc<dyn ShellSpawner> = Arc::new(SpawnerConfig(FakeSpawner {
            chunks: vec!["boom".to_string()],
            exit: Some(Some(7)),
            killed: Arc::new(Mutex::new(0)),
        }));
        let dir = temp("run-exit");
        let tool = shell_tool(spawner, TRUNC_MAX_BYTES, &dir);
        let (result, _, _, _) = execute(&tool, json!({ "command": "exit 7" })).await;
        assert_eq!(result.metadata["exit"], json!(7));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn run_timeout_kills_and_reports() {
        let spawner: Arc<dyn ShellSpawner> = Arc::new(SpawnerConfig(FakeSpawner {
            chunks: vec!["partial output\n".to_string()],
            exit: None,
            killed: Arc::new(Mutex::new(0)),
        }));
        let dir = temp("run-timeout");
        let tool = shell_tool(spawner, TRUNC_MAX_BYTES, &dir);
        let (result, _, _, _) =
            execute(&tool, json!({ "command": "sleep 100", "timeout": 100 })).await;
        assert_eq!(
            result.output,
            "partial output\n\n\n<shell_metadata>\nshell tool terminated command after exceeding timeout 100 ms. If this command is expected to take longer and is not waiting for interactive input, retry with a larger timeout value in milliseconds.\n</shell_metadata>"
        );
        assert_eq!(result.metadata["exit"], serde_json::Value::Null);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn run_negative_timeout_is_invalid() {
        let spawner: Arc<dyn ShellSpawner> = Arc::new(SpawnerConfig(FakeSpawner {
            chunks: vec![],
            exit: Some(Some(0)),
            killed: Arc::new(Mutex::new(0)),
        }));
        let dir = temp("run-neg");
        let tool = shell_tool(spawner, TRUNC_MAX_BYTES, &dir);
        let ask = RecordingAsk::new();
        let instance = InstanceContext {
            directory: std::env::temp_dir(),
            worktree: Path::new("/").to_path_buf(),
        };
        let extra = Extra::default();
        let def = tool.def(Arc::new(FixedAgents)).await;
        let ctx = ToolCtxRef {
            session_id: "ses_1",
            message_id: "msg_1",
            agent: "build",
            call_id: None,
            abort: tokio_util::sync::CancellationToken::new(),
            messages: &[],
            extra: &extra,
            instance: &instance,
            ask: &ask,
            metadata: &ask,
        };
        let err = (def.execute)(json!({ "command": "x", "timeout": -1 }), ctx)
            .await
            .expect_err("negative timeout");
        assert_eq!(
            err.to_string(),
            "Invalid timeout value: -1. Timeout must be a positive number."
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn run_spill_file_written_with_full_output() {
        // max_bytes = 32: the second chunk pushes `full` over the limit.
        let first = "a".repeat(20);
        let second = "b".repeat(20);
        let third = "c".repeat(20);
        let spawner: Arc<dyn ShellSpawner> = Arc::new(SpawnerConfig(FakeSpawner {
            chunks: vec![first.clone(), second.clone(), third.clone()],
            exit: Some(Some(0)),
            killed: Arc::new(Mutex::new(0)),
        }));
        let dir = temp("run-spill");
        let tool = shell_tool(spawner, 32, &dir);
        let (result, _, _, _) = execute(&tool, json!({ "command": "cat big" })).await;
        assert_eq!(result.metadata["truncated"], json!(true));
        let output_path = result.metadata["outputPath"].as_str().expect("outputPath");
        let spilled = std::fs::read_to_string(output_path).expect("spill file");
        assert_eq!(spilled, format!("{first}{second}{third}"));
        assert!(result
            .output
            .starts_with("...output truncated...\n\nFull output saved to: "));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn run_window_eviction_sets_truncated() {
        // keep = max_bytes * 2; enough small chunks evict the front.
        let chunks: Vec<String> = (0..20).map(|i| format!("chunk-{i:02}\n")).collect();
        let spawner: Arc<dyn ShellSpawner> = Arc::new(SpawnerConfig(FakeSpawner {
            chunks,
            exit: Some(Some(0)),
            killed: Arc::new(Mutex::new(0)),
        }));
        let dir = temp("run-window");
        let tool = shell_tool(spawner, 32, &dir);
        let (result, _, _, _) = execute(&tool, json!({ "command": "cat many" })).await;
        assert_eq!(result.metadata["truncated"], json!(true));
        assert!(result.output.starts_with("...output truncated..."));
        assert!(result.output.contains("chunk-19\n"));
        assert!(!result.output.contains("chunk-00"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn run_aborts_on_cancelled_token() {
        let spawner: Arc<dyn ShellSpawner> = Arc::new(SpawnerConfig(FakeSpawner {
            chunks: vec!["waiting\n".to_string()],
            exit: None,
            killed: Arc::new(Mutex::new(0)),
        }));
        let dir = temp("run-abort");
        let tool = shell_tool(spawner, TRUNC_MAX_BYTES, &dir);
        let ask = RecordingAsk::new();
        let instance = InstanceContext {
            directory: std::env::temp_dir(),
            worktree: Path::new("/").to_path_buf(),
        };
        let extra = Extra::default();
        let abort = tokio_util::sync::CancellationToken::new();
        let def = tool.def(Arc::new(FixedAgents)).await;
        let ctx = ToolCtxRef {
            session_id: "ses_1",
            message_id: "msg_1",
            agent: "build",
            call_id: None,
            abort: abort.clone(),
            messages: &[],
            extra: &extra,
            instance: &instance,
            ask: &ask,
            metadata: &ask,
        };
        abort.cancel();
        let result = (def.execute)(json!({ "command": "sleep 100" }), ctx)
            .await
            .expect("execute succeeds");
        assert!(result.output.contains("User aborted the command"));
        assert_eq!(result.metadata["exit"], serde_json::Value::Null);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn run_ask_order_external_directory_before_bash() {
        let spawner: Arc<dyn ShellSpawner> = Arc::new(SpawnerConfig(FakeSpawner {
            chunks: vec![],
            exit: Some(Some(0)),
            killed: Arc::new(Mutex::new(0)),
        }));
        let dir = temp("run-ask-order");
        let tool = shell_tool(spawner, TRUNC_MAX_BYTES, &dir);
        let ask = RecordingAsk::new();
        let instance = InstanceContext {
            directory: std::env::temp_dir(),
            worktree: Path::new("/").to_path_buf(),
        };
        let extra = Extra::default();
        let def = tool.def(Arc::new(FixedAgents)).await;
        let ctx = ToolCtxRef {
            session_id: "ses_1",
            message_id: "msg_1",
            agent: "build",
            call_id: None,
            abort: tokio_util::sync::CancellationToken::new(),
            messages: &[],
            extra: &extra,
            instance: &instance,
            ask: &ask,
            metadata: &ask,
        };
        let result = (def.execute)(json!({ "command": "rm -rf /external/thing" }), ctx)
            .await
            .expect("execute succeeds");
        assert_eq!(result.metadata["exit"], json!(0));
        let requests = ask.requests.lock().unwrap();
        assert_eq!(requests.len(), 2, "two asks expected");
        assert_eq!(requests[0].permission, "external_directory");
        assert_eq!(requests[0].patterns, vec!["/external/*".to_string()]);
        assert_eq!(requests[1].permission, "bash");
        assert_eq!(
            requests[1].metadata,
            json!({ "command": "rm -rf /external/thing" })
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn run_cd_only_command_asks_nothing() {
        let spawner: Arc<dyn ShellSpawner> = Arc::new(SpawnerConfig(FakeSpawner {
            chunks: vec![],
            exit: Some(Some(0)),
            killed: Arc::new(Mutex::new(0)),
        }));
        let dir = temp("run-cd");
        let tool = shell_tool(spawner, TRUNC_MAX_BYTES, &dir);
        let (result, ask, _, _) = execute(&tool, json!({ "command": "cd sub" })).await;
        assert!(ask.requests.lock().unwrap().is_empty());
        assert_eq!(result.output, "(no output)");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn run_workdir_outside_instance_adds_cwd_dir() {
        let spawner: Arc<dyn ShellSpawner> = Arc::new(SpawnerConfig(FakeSpawner {
            chunks: vec![],
            exit: Some(Some(0)),
            killed: Arc::new(Mutex::new(0)),
        }));
        let dir = temp("run-workdir");
        let tool = shell_tool(spawner, TRUNC_MAX_BYTES, &dir);
        let ask = RecordingAsk::new();
        let instance = InstanceContext {
            directory: std::env::temp_dir(),
            worktree: Path::new("/").to_path_buf(),
        };
        let extra = Extra::default();
        let def = tool.def(Arc::new(FixedAgents)).await;
        let ctx = ToolCtxRef {
            session_id: "ses_1",
            message_id: "msg_1",
            agent: "build",
            call_id: None,
            abort: tokio_util::sync::CancellationToken::new(),
            messages: &[],
            extra: &extra,
            instance: &instance,
            ask: &ask,
            metadata: &ask,
        };
        let result = (def.execute)(json!({ "command": "cd .", "workdir": "/outside/dir" }), ctx)
            .await
            .expect("execute succeeds");
        assert_eq!(result.metadata["exit"], json!(0));
        let requests = ask.requests.lock().unwrap();
        assert_eq!(requests[0].permission, "external_directory");
        assert!(requests[0].patterns.contains(&"/outside/dir/*".to_string()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn run_real_spawner_echo() {
        let spawner: Arc<dyn ShellSpawner> = Arc::new(TokioSpawner);
        let dir = temp("run-real");
        let tool = shell_tool(spawner, TRUNC_MAX_BYTES, &dir);
        let (result, _, _, _) =
            execute(&tool, json!({ "command": "echo opencode-real-echo-test" })).await;
        assert_eq!(result.output.trim(), "opencode-real-echo-test");
        assert_eq!(result.metadata["exit"], json!(0));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn run_real_spawner_timeout_kill() {
        let spawner: Arc<dyn ShellSpawner> = Arc::new(TokioSpawner);
        let dir = temp("run-real-kill");
        let tool = shell_tool(spawner, TRUNC_MAX_BYTES, &dir);
        let ask = RecordingAsk::new();
        let instance = InstanceContext {
            directory: std::env::temp_dir(),
            worktree: Path::new("/").to_path_buf(),
        };
        let extra = Extra::default();
        let def = tool.def(Arc::new(FixedAgents)).await;
        let ctx = ToolCtxRef {
            session_id: "ses_1",
            message_id: "msg_1",
            agent: "build",
            call_id: None,
            abort: tokio_util::sync::CancellationToken::new(),
            messages: &[],
            extra: &extra,
            instance: &instance,
            ask: &ask,
            metadata: &ask,
        };
        let result = (def.execute)(json!({ "command": "sleep 60", "timeout": 200 }), ctx)
            .await
            .expect("execute succeeds");
        assert!(result
            .output
            .contains("shell tool terminated command after exceeding timeout 200 ms."));
        assert_eq!(result.metadata["exit"], serde_json::Value::Null);
        std::fs::remove_dir_all(&dir).ok();
    }
}

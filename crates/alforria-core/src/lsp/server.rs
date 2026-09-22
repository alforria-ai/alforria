//! Language-server registry — port of `lsp/server.ts`.
//!
//! Deviation from the TS reference: the `disableLspDownload` branches
//! that fetch/install language servers from the network
//! (`flags.disableLspDownload` guards at `server.ts:152`, `:182`, `:370`,
//! `:404`, `:493`, `:550`, `:598`, `:692`, `:737` and the `Npm.which`
//! install fallbacks) are not ported — when the binary is not found
//! locally, spawn returns `None` (the TS behavior under
//! `OPENCODE_DISABLE_LSP_DOWNLOAD=1`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;

use crate::lsp::launch;
use crate::tool::def::BoxFuture;

/// `RuntimeFlags` (runtime-flags.ts:22,44).
#[derive(Debug, Clone, Copy, Default)]
pub struct Flags {
    pub disable_lsp_download: bool,
    pub experimental_lsp_ty: bool,
}

impl Flags {
    /// Read the flag envs (evaluated once per instance boot).
    pub fn from_env() -> Flags {
        let bool_env = |name: &str| {
            std::env::var(name)
                .map(|value| matches!(value.as_str(), "1" | "true" | "yes"))
                .unwrap_or(false)
        };
        Flags {
            disable_lsp_download: bool_env("OPENCODE_DISABLE_LSP_DOWNLOAD"),
            experimental_lsp_ty: bool_env("OPENCODE_EXPERIMENTAL_LSP_TY"),
        }
    }
}

/// Everything the root/spawn closures close over — the instance context
/// plus `Global.Path.bin`.
#[derive(Debug, Clone)]
pub struct ServerContext {
    pub directory: PathBuf,
    pub worktree: PathBuf,
    pub bin: PathBuf,
    pub flags: Flags,
}

/// `Handle` (server.ts:25-28) — a spawned language server process.
pub struct Handle {
    pub child: tokio::process::Child,
    pub initialization: Option<serde_json::Map<String, Value>>,
}

pub type RootFn =
    Arc<dyn Fn(&str, Arc<ServerContext>) -> BoxFuture<'static, Option<PathBuf>> + Send + Sync>;
pub type SpawnFn =
    Arc<dyn Fn(PathBuf, Arc<ServerContext>) -> BoxFuture<'static, Option<Handle>> + Send + Sync>;

/// `Info` (server.ts:80-86).
#[derive(Clone)]
pub struct ServerInfo {
    pub id: String,
    pub extensions: Vec<String>,
    pub root: RootFn,
    pub spawn: SpawnFn,
}

async fn exists(path: &Path) -> bool {
    tokio::fs::metadata(path).await.is_ok()
}

async fn read_text(path: &Path) -> Option<String> {
    tokio::fs::read_to_string(path).await.ok()
}

fn dirname_of(file: &str) -> PathBuf {
    let path = Path::new(file);
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// `Filesystem.up` (util/filesystem.ts:213-224) — first match walking up
/// from `start` to `stop` (inclusive).
async fn up_first(targets: &[&str], start: &Path, stop: &Path) -> Option<PathBuf> {
    let mut current = start.to_path_buf();
    loop {
        for target in targets {
            let candidate = current.join(target);
            if exists(&candidate).await {
                return Some(candidate);
            }
        }
        if current == stop {
            break;
        }
        let Some(parent) = current.parent() else {
            break;
        };
        let parent = parent.to_path_buf();
        if parent == current {
            break;
        }
        current = parent;
    }
    None
}

fn parent_of(path: PathBuf) -> PathBuf {
    path.parent()
        .map(|p| p.to_path_buf())
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `NearestRoot` (server.ts:32-54) — falls back to `ctx.directory`.
pub fn nearest_root(include: &[&'static str], exclude: Option<&[&'static str]>) -> RootFn {
    let include = include.to_vec();
    let exclude = exclude.map(|list| list.to_vec());
    Arc::new(move |file, ctx| {
        let include = include.clone();
        let exclude = exclude.clone();
        let file = file.to_string();
        Box::pin(async move {
            if let Some(exclude) = &exclude {
                if up_first(exclude, &dirname_of(&file), &ctx.directory)
                    .await
                    .is_some()
                {
                    return None;
                }
            }
            match up_first(&include, &dirname_of(&file), &ctx.directory).await {
                Some(found) => Some(parent_of(found)),
                None => Some(ctx.directory.clone()),
            }
        })
    })
}

/// `StrictNearestRoot` (server.ts:56-78).
pub fn strict_nearest_root(include: &[&'static str], exclude: Option<&[&'static str]>) -> RootFn {
    let include = include.to_vec();
    let exclude = exclude.map(|list| list.to_vec());
    Arc::new(move |file, ctx| {
        let include = include.clone();
        let exclude = exclude.clone();
        let file = file.to_string();
        Box::pin(async move {
            if let Some(exclude) = &exclude {
                if up_first(exclude, &dirname_of(&file), &ctx.directory)
                    .await
                    .is_some()
                {
                    return None;
                }
            }
            up_first(&include, &dirname_of(&file), &ctx.directory)
                .await
                .map(parent_of)
        })
    })
}

/// `which` (util/which.ts) — PATH search with `Global.Path.bin` appended.
pub fn which(cmd: &str, ctx: &ServerContext) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> =
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).collect();
    dirs.push(ctx.bin.clone());
    for dir in dirs {
        let candidate = dir.join(cmd);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// `Module.resolve` (util/module.ts) — resolve a module path walking
/// `node_modules` up from `dir`.
pub fn module_resolve(id: &str, dir: &Path) -> Option<PathBuf> {
    let mut current = dir.to_path_buf();
    loop {
        let candidate = current.join("node_modules").join(id);
        if candidate.exists() {
            return Some(candidate);
        }
        let parent = current.parent()?.to_path_buf();
        if parent == current {
            return None;
        }
        current = parent;
    }
}

/// `Npm.which` (npm.ts:192-225) — the already-installed check against
/// `<cache>/packages/<pkg>/node_modules/.bin`; the install fallback is
/// not ported (see the module docs).
pub fn npm_which(pkg: &str, bin: Option<&str>, ctx: &ServerContext) -> Option<PathBuf> {
    let cache = ctx.bin.parent()?;
    let dir = cache.join("packages").join(pkg);
    let bin_dir = dir.join("node_modules").join(".bin");
    let entries = std::fs::read_dir(&bin_dir).ok()?;
    let files: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_file())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    if files.is_empty() {
        return None;
    }
    if let Some(bin) = bin {
        return files
            .iter()
            .find(|file| file.as_str() == bin)
            .map(|file| bin_dir.join(file));
    }
    if files.len() == 1 {
        return Some(bin_dir.join(&files[0]));
    }
    let pkg_json = std::fs::read(dir.join("node_modules").join(pkg).join("package.json")).ok()?;
    let pkg_json: Value = serde_json::from_slice(&pkg_json).ok()?;
    let unscoped = pkg.split('/').next_back().unwrap_or(pkg);
    if let Some(bin_value) = pkg_json.get("bin").filter(|bin| !bin.is_null()) {
        if let Some(name) = bin_value.as_str() {
            return Some(bin_dir.join(name));
        }
        if let Some(keys) = bin_value.as_object() {
            if let Some(first) = keys.keys().next() {
                if keys.len() == 1 || keys.contains_key(unscoped) {
                    return Some(bin_dir.join(first));
                }
            }
        }
    }
    Some(bin_dir.join(&files[0]))
}

fn root_fn(
    f: impl Fn(&str, Arc<ServerContext>) -> BoxFuture<'static, Option<PathBuf>> + Send + Sync + 'static,
) -> RootFn {
    Arc::new(f)
}

fn spawn_fn(
    f: impl Fn(PathBuf, Arc<ServerContext>) -> BoxFuture<'static, Option<Handle>>
        + Send
        + Sync
        + 'static,
) -> SpawnFn {
    Arc::new(f)
}

fn spawn_bin(bin: &Path, args: &[String], root: &Path) -> Option<Handle> {
    launch::spawn(&bin.to_string_lossy(), args, root, None).ok()
}

fn spawn_with(
    bin: &Path,
    args: &[String],
    root: &Path,
    env: Option<&std::collections::BTreeMap<String, String>>,
    initialization: Option<serde_json::Map<String, Value>>,
) -> Option<Handle> {
    let mut handle = launch::spawn(&bin.to_string_lossy(), args, root, env).ok()?;
    handle.initialization = initialization;
    Some(handle)
}

/// Run a short command and capture its stdout (the TS `text()` /
/// `output()` helpers).
async fn run_capture(cmd: &str, args: &[&str]) -> Option<String> {
    let output = tokio::process::Command::new(cmd)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The Roslyn language-server resolution (`server.ts:737-780`) — the
/// local parts; the `dotnet tool install` download branch is not ported.
async fn get_roslyn_language_server(ctx: &ServerContext) -> Option<PathBuf> {
    if let Some(bin) = which("roslyn-language-server", ctx) {
        return Some(bin);
    }
    let home = std::env::var_os("DOTNET_CLI_HOME")
        .map(PathBuf::from)
        .or_else(dirs::home_dir)?;
    let bin = home
        .join(".dotnet")
        .join("tools")
        .join("roslyn-language-server");
    if is_executable(&bin) {
        Some(bin)
    } else {
        None
    }
}

/// `findVscodeRazorExtension` (server.ts:798-849).
async fn find_vscode_razor_extension() -> Option<Value> {
    let home = dirs::home_dir()?;
    let roots = [
        std::env::var_os("VSCODE_EXTENSIONS").map(PathBuf::from),
        Some(home.join(".vscode").join("extensions")),
        Some(home.join(".vscode-insiders").join("extensions")),
        Some(home.join(".vscode-server").join("extensions")),
        Some(home.join(".vscode-server-insiders").join("extensions")),
    ];
    for root in roots.into_iter().flatten() {
        let entries = std::fs::read_dir(&root).ok()?;
        let mut candidates = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.path().is_dir() && name.starts_with("ms-dotnettools.csharp-") {
                let modified = entry
                    .metadata()
                    .ok()
                    .and_then(|metadata| metadata.modified().ok())
                    .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|time| time.as_millis());
                candidates.push((entry.path(), modified));
            }
        }
        candidates.sort_by_key(|(_, modified)| *modified);
        for (path, _) in candidates.iter().rev() {
            let result = serde_json::json!({
                "compiler": path.join("Microsoft.CodeAnalysis.Razor.Compiler.dll"),
                "targets": path.join("Targets").join("Microsoft.NET.Sdk.Razor.DesignTime.targets"),
                "extension": path.join("Microsoft.VisualStudioCode.RazorExtension.dll"),
            });
            let exists = |value: &Value| Path::new(value.as_str().expect("path")).exists();
            if exists(&result["compiler"])
                && exists(&result["targets"])
                && exists(&result["extension"])
            {
                return Some(result);
            }
        }
    }
    None
}

/// `Object.values(LSPServer)` — every built-in server, in export order
/// (server.ts). The iteration order selects which servers get spawned for
/// a file.
pub fn builtin_servers() -> Vec<ServerInfo> {
    let server = |id: &str, extensions: &[&str], root: RootFn, spawn: SpawnFn| ServerInfo {
        id: id.to_string(),
        extensions: extensions.iter().map(|ext| ext.to_string()).collect(),
        root,
        spawn,
    };
    vec![
        server(
            "deno",
            &[".ts", ".tsx", ".js", ".jsx", ".mjs"],
            root_fn(|file, ctx| {
                let file = file.to_string();
                Box::pin(async move {
                    up_first(
                        &["deno.json", "deno.jsonc"],
                        &dirname_of(&file),
                        &ctx.directory,
                    )
                    .await
                    .map(parent_of)
                })
            }),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    let deno = which("deno", &ctx)?;
                    spawn_bin(&deno, &["lsp".to_string()], &root)
                })
            }),
        ),
        server(
            "typescript",
            &[".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts"],
            nearest_root(
                &[
                    "package-lock.json",
                    "bun.lockb",
                    "bun.lock",
                    "pnpm-lock.yaml",
                    "yarn.lock",
                ],
                Some(&["deno.json", "deno.jsonc"]),
            ),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    let tsserver = module_resolve("typescript/lib/tsserver.js", &ctx.directory)?;
                    let bin = npm_which("typescript-language-server", None, &ctx)?;
                    let initialization = serde_json::json!({ "tsserver": { "path": tsserver } });
                    let mut handle = spawn_bin(&bin, &["--stdio".to_string()], &root)?;
                    handle.initialization =
                        Some(initialization.as_object().expect("object").clone());
                    Some(handle)
                })
            }),
        ),
        server(
            "vue",
            &[".vue"],
            nearest_root(
                &[
                    "package-lock.json",
                    "bun.lockb",
                    "bun.lock",
                    "pnpm-lock.yaml",
                    "yarn.lock",
                ],
                None,
            ),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    let bin = match which("vue-language-server", &ctx) {
                        Some(bin) => bin,
                        None => npm_which("@vue/language-server", None, &ctx)?,
                    };
                    spawn_with(
                        &bin,
                        &["--stdio".to_string()],
                        &root,
                        None,
                        Some(Default::default()),
                    )
                })
            }),
        ),
        server(
            "eslint",
            &[
                ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts", ".vue",
            ],
            nearest_root(
                &[
                    "package-lock.json",
                    "bun.lockb",
                    "bun.lock",
                    "pnpm-lock.yaml",
                    "yarn.lock",
                ],
                None,
            ),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    module_resolve("eslint", &ctx.directory)?;
                    let server_path = ctx
                        .bin
                        .join("vscode-eslint")
                        .join("server")
                        .join("out")
                        .join("eslintServer.js");
                    if !exists(&server_path).await {
                        return None;
                    }
                    let node = which("node", &ctx)?;
                    let args = vec![
                        server_path.to_string_lossy().into_owned(),
                        "--stdio".to_string(),
                    ];
                    spawn_bin(&node, &args, &root)
                })
            }),
        ),
        server(
            "oxlint",
            &[
                ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts", ".vue", ".astro",
                ".svelte",
            ],
            nearest_root(
                &[
                    ".oxlintrc.json",
                    "package-lock.json",
                    "bun.lockb",
                    "bun.lock",
                    "pnpm-lock.yaml",
                    "yarn.lock",
                    "package.json",
                ],
                None,
            ),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    let resolve_bin = |target: &str| -> Option<PathBuf> {
                        let local_bin = root.join(target);
                        if local_bin.exists() {
                            return Some(local_bin);
                        }
                        let mut current = root.clone();
                        loop {
                            let candidate = current.join(target);
                            if candidate.exists() {
                                return Some(candidate);
                            }
                            let Some(parent) = current.parent() else {
                                break;
                            };
                            if current == ctx.worktree || parent == current {
                                break;
                            }
                            current = parent.to_path_buf();
                        }
                        None
                    };
                    let lint_target = Path::new("node_modules").join(".bin").join("oxlint");
                    let mut lint_bin = resolve_bin(&lint_target.to_string_lossy());
                    if lint_bin.is_none() {
                        lint_bin = which("oxlint", &ctx);
                    }
                    if let Some(bin) = lint_bin {
                        let output = run_capture(&bin.to_string_lossy(), &["--help"]).await;
                        if let Some(output) = output {
                            if output.contains("--lsp") {
                                return spawn_bin(&bin, &["--lsp".to_string()], &root);
                            }
                        }
                    }
                    let server_target = Path::new("node_modules")
                        .join(".bin")
                        .join("oxc_language_server");
                    let mut server_bin = resolve_bin(&server_target.to_string_lossy());
                    if server_bin.is_none() {
                        server_bin = which("oxc_language_server", &ctx);
                    }
                    let bin = server_bin?;
                    spawn_bin(&bin, &[], &root)
                })
            }),
        ),
        server(
            "biome",
            &[
                ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".mts", ".cts", ".json", ".jsonc",
                ".vue", ".astro", ".svelte", ".css", ".graphql", ".gql", ".html",
            ],
            nearest_root(
                &[
                    "biome.json",
                    "biome.jsonc",
                    "package-lock.json",
                    "bun.lockb",
                    "bun.lock",
                    "pnpm-lock.yaml",
                    "yarn.lock",
                ],
                None,
            ),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    let local_bin = root.join("node_modules").join(".bin").join("biome");
                    let mut bin = None;
                    if exists(&local_bin).await {
                        bin = Some(local_bin);
                    }
                    if bin.is_none() {
                        bin = which("biome", &ctx);
                    }
                    if bin.is_none() {
                        module_resolve("biome", &root)?;
                        bin = npm_which("biome", None, &ctx);
                    }
                    let bin = bin?;
                    spawn_bin(
                        &bin,
                        &["lsp-proxy".to_string(), "--stdio".to_string()],
                        &root,
                    )
                })
            }),
        ),
        server(
            "gopls",
            &[".go"],
            nearest_root(&["go.work"], None),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    let bin = which("gopls", &ctx)?;
                    spawn_bin(&bin, &[], &root)
                })
            }),
        ),
        server(
            "ruby-lsp",
            &[".rb", ".rake", ".gemspec", ".ru"],
            nearest_root(&["Gemfile"], None),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    let bin = which("rubocop", &ctx)?;
                    spawn_bin(&bin, &["--lsp".to_string()], &root)
                })
            }),
        ),
        server(
            "ty",
            &[".py", ".pyi"],
            nearest_root(
                &[
                    "pyproject.toml",
                    "ty.toml",
                    "setup.py",
                    "setup.cfg",
                    "requirements.txt",
                    "Pipfile",
                    "pyrightconfig.json",
                ],
                None,
            ),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    if !ctx.flags.experimental_lsp_ty {
                        return None;
                    }
                    let venvs: Vec<PathBuf> = [
                        std::env::var_os("VIRTUAL_ENV").map(PathBuf::from),
                        Some(root.join(".venv")),
                        Some(root.join("venv")),
                    ]
                    .into_iter()
                    .flatten()
                    .collect();
                    let mut initialization = serde_json::Map::new();
                    for venv in &venvs {
                        let python = venv.join("bin").join("python");
                        if python.exists() {
                            initialization.insert(
                                "pythonPath".to_string(),
                                Value::String(python.to_string_lossy().into_owned()),
                            );
                            break;
                        }
                    }
                    let bin = match which("ty", &ctx) {
                        Some(bin) => Some(bin),
                        None => venvs
                            .iter()
                            .map(|venv| venv.join("bin").join("ty"))
                            .find(|bin| bin.exists()),
                    };
                    let bin = bin?;
                    let mut handle = spawn_bin(&bin, &["server".to_string()], &root)?;
                    handle.initialization = Some(initialization);
                    Some(handle)
                })
            }),
        ),
        server(
            "pyright",
            &[".py", ".pyi"],
            nearest_root(
                &[
                    "pyproject.toml",
                    "setup.py",
                    "setup.cfg",
                    "requirements.txt",
                    "Pipfile",
                    "pyrightconfig.json",
                ],
                None,
            ),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    let bin = match which("pyright-langserver", &ctx) {
                        Some(bin) => bin,
                        None => npm_which("pyright", Some("pyright-langserver"), &ctx)?,
                    };
                    let venvs: Vec<PathBuf> = [
                        std::env::var_os("VIRTUAL_ENV").map(PathBuf::from),
                        Some(root.join(".venv")),
                        Some(root.join("venv")),
                    ]
                    .into_iter()
                    .flatten()
                    .collect();
                    let mut initialization = serde_json::Map::new();
                    for venv in &venvs {
                        let python = venv.join("bin").join("python");
                        if python.exists() {
                            initialization.insert(
                                "pythonPath".to_string(),
                                Value::String(python.to_string_lossy().into_owned()),
                            );
                            break;
                        }
                    }
                    spawn_with(
                        &bin,
                        &["--stdio".to_string()],
                        &root,
                        None,
                        Some(initialization),
                    )
                })
            }),
        ),
        server(
            "elixir-ls",
            &[".ex", ".exs"],
            nearest_root(&["mix.exs", "mix.lock"], None),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    let mut bin = which("elixir-ls", &ctx);
                    if bin.is_none() {
                        let installed = ctx
                            .bin
                            .join("elixir-ls-master")
                            .join("release")
                            .join("language_server.sh");
                        if exists(&installed).await {
                            bin = Some(installed);
                        }
                    }
                    let bin = bin?;
                    spawn_bin(&bin, &[], &root)
                })
            }),
        ),
        server(
            "zls",
            &[".zig", ".zon"],
            nearest_root(&["build.zig"], None),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    let bin = which("zls", &ctx)?;
                    spawn_bin(&bin, &[], &root)
                })
            }),
        ),
        server(
            "csharp",
            &[".cs", ".csx"],
            nearest_root(&[".slnx", ".sln", ".csproj", "global.json"], None),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    let bin = get_roslyn_language_server(&ctx).await?;
                    spawn_bin(
                        &bin,
                        &["--stdio".to_string(), "--autoLoadProjects".to_string()],
                        &root,
                    )
                })
            }),
        ),
        server(
            "razor",
            &[".razor", ".cshtml"],
            nearest_root(&[".slnx", ".sln", ".csproj", "global.json"], None),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    let bin = get_roslyn_language_server(&ctx).await?;
                    let razor = find_vscode_razor_extension().await?;
                    let compiler = razor["compiler"].as_str()?;
                    let targets = razor["targets"].as_str()?;
                    let extension = razor["extension"].as_str()?;
                    spawn_bin(
                        &bin,
                        &[
                            "--stdio".to_string(),
                            "--autoLoadProjects".to_string(),
                            format!("--razorSourceGenerator={compiler}"),
                            format!("--razorDesignTimePath={targets}"),
                            "--extension".to_string(),
                            extension.to_string(),
                        ],
                        &root,
                    )
                })
            }),
        ),
        server(
            "fsharp",
            &[".fs", ".fsi", ".fsx", ".fsscript"],
            nearest_root(&[".slnx", ".sln", ".fsproj", "global.json"], None),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    let bin = which("fsautocomplete", &ctx)?;
                    spawn_bin(&bin, &[], &root)
                })
            }),
        ),
        server(
            "sourcekit-lsp",
            &[".swift", ".objc", "objcpp"],
            nearest_root(&["Package.swift", "*.xcodeproj", "*.xcworkspace"], None),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    if let Some(bin) = which("sourcekit-lsp", &ctx) {
                        return spawn_bin(&bin, &[], &root);
                    }
                    which("xcrun", &ctx)?;
                    let output = run_capture("xcrun", &["--find", "sourcekit-lsp"]).await?;
                    let bin = output.trim();
                    if bin.is_empty() {
                        return None;
                    }
                    spawn_bin(&PathBuf::from(bin), &[], &root)
                })
            }),
        ),
        server(
            "rust",
            &[".rs"],
            root_fn(|file, ctx| {
                let file = file.to_string();
                Box::pin(async move {
                    // NearestRoot never returns undefined (ctx.directory fallback).
                    let crate_root = match up_first(
                        &["Cargo.toml", "Cargo.lock"],
                        &dirname_of(&file),
                        &ctx.directory,
                    )
                    .await
                    {
                        Some(found) => parent_of(found),
                        None => ctx.directory.clone(),
                    };
                    let mut current_dir = crate_root.clone();
                    while let Some(parent_dir) = current_dir.parent() {
                        let cargo_toml = current_dir.join("Cargo.toml");
                        if let Some(content) = read_text(&cargo_toml).await {
                            if content.contains("[workspace]") {
                                return Some(current_dir);
                            }
                        }
                        if parent_dir == current_dir {
                            break;
                        }
                        current_dir = parent_dir.to_path_buf();
                        if !current_dir.starts_with(&ctx.worktree) {
                            break;
                        }
                    }
                    Some(crate_root)
                })
            }),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    let bin = which("rust-analyzer", &ctx)?;
                    spawn_bin(&bin, &[], &root)
                })
            }),
        ),
        server(
            "clangd",
            &[
                ".c", ".cpp", ".cc", ".cxx", ".c++", ".h", ".hpp", ".hh", ".hxx", ".h++",
            ],
            nearest_root(
                &["compile_commands.json", "compile_flags.txt", ".clangd"],
                None,
            ),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    let args = vec!["--background-index".to_string(), "--clang-tidy".to_string()];
                    if let Some(from_path) = which("clangd", &ctx) {
                        return spawn_bin(&from_path, &args, &root);
                    }
                    let entries = std::fs::read_dir(&ctx.bin).ok();
                    if let Some(entries) = entries {
                        for entry in entries.flatten() {
                            if !entry.path().is_dir() {
                                continue;
                            }
                            if !entry.file_name().to_string_lossy().starts_with("clangd_") {
                                continue;
                            }
                            let candidate = entry.path().join("bin").join("clangd");
                            if exists(&candidate).await {
                                return spawn_bin(&candidate, &args, &root);
                            }
                        }
                    }
                    None
                })
            }),
        ),
        server(
            "svelte",
            &[".svelte"],
            nearest_root(
                &[
                    "package-lock.json",
                    "bun.lockb",
                    "bun.lock",
                    "pnpm-lock.yaml",
                    "yarn.lock",
                ],
                None,
            ),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    let bin = match which("svelteserver", &ctx) {
                        Some(bin) => bin,
                        None => npm_which("svelte-language-server", None, &ctx)?,
                    };
                    spawn_with(
                        &bin,
                        &["--stdio".to_string()],
                        &root,
                        None,
                        Some(Default::default()),
                    )
                })
            }),
        ),
        server(
            "astro",
            &[".astro"],
            nearest_root(
                &[
                    "package-lock.json",
                    "bun.lockb",
                    "bun.lock",
                    "pnpm-lock.yaml",
                    "yarn.lock",
                ],
                None,
            ),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    let tsserver = module_resolve("typescript/lib/tsserver.js", &ctx.directory)?;
                    let tsdk = Path::new(&tsserver)
                        .parent()
                        .map(|p| p.to_path_buf())
                        .unwrap_or_default();
                    let bin = match which("astro-ls", &ctx) {
                        Some(bin) => bin,
                        None => npm_which("@astrojs/language-server", None, &ctx)?,
                    };
                    let initialization = serde_json::json!({ "typescript": { "tsdk": tsdk } });
                    spawn_with(
                        &bin,
                        &["--stdio".to_string()],
                        &root,
                        None,
                        Some(initialization.as_object().expect("object").clone()),
                    )
                })
            }),
        ),
        server(
            "jdtls",
            &[".java"],
            root_fn(|file, ctx| {
                let file = file.to_string();
                Box::pin(async move {
                    let settings_markers = ["settings.gradle", "settings.gradle.kts"];
                    let gradle_markers = ["gradlew", "gradlew.bat"];
                    let wrapper_root = strict_nearest_root(
                        &gradle_markers,
                        Some(&settings_markers),
                    )(&file, ctx.clone())
                    .await;
                    if wrapper_root.is_some() {
                        return wrapper_root;
                    }
                    let settings_root =
                        strict_nearest_root(&settings_markers, None)(&file, ctx.clone()).await;
                    if settings_root.is_some() {
                        return settings_root;
                    }
                    let build_root = strict_nearest_root(
                        &["build.gradle", "build.gradle.kts"],
                        None,
                    )(&file, ctx.clone())
                    .await;
                    if build_root.is_some() {
                        return build_root;
                    }
                    let pom_files =
                        find_up_all("pom.xml", &dirname_of(&file), &ctx.directory).await;
                    if !pom_files.is_empty() {
                        let mut root = parent_of(pom_files[0].clone());
                        for pom in pom_files.iter().skip(1) {
                            let parent_dir = parent_of(pom.clone());
                            let rel = parent_dir
                                .strip_prefix(&root)
                                .map(|p| p.to_string_lossy().into_owned())
                                .unwrap_or_default();
                            if let Some(content) = read_text(pom).await {
                                if is_module_of(&content, &rel) {
                                    root = parent_dir;
                                } else {
                                    break;
                                }
                            } else {
                                break;
                            }
                        }
                        return Some(root);
                    }
                    strict_nearest_root(&[".project", ".classpath"], None)(&file, ctx.clone()).await
                })
            }),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    let java = which("java", &ctx)?;
                    let version = run_capture(&java.to_string_lossy(), &["-version"]).await;
                    let version = match version {
                        Some(version) if version.trim().is_empty() => None,
                        version => version,
                    };
                    let version = version.filter(|v| !v.trim().is_empty())?;
                    let major = java_major_version(&version).unwrap_or(0);
                    if major < 21 {
                        return None;
                    }
                    let dist_path = ctx.bin.join("jdtls");
                    let launcher_dir = dist_path.join("plugins");
                    if !exists(&launcher_dir).await {
                        return None;
                    }
                    let launcher_jar =
                        std::fs::read_dir(&launcher_dir).ok().and_then(|entries| {
                            entries.flatten().map(|entry| entry.path()).find(|path| {
                                let name =
                                    path.file_name().map(|n| n.to_string_lossy().into_owned());
                                name.is_some_and(|name| {
                                    name.starts_with("org.eclipse.equinox.launcher_")
                                        && name.ends_with(".jar")
                                })
                            })
                        })?;
                    if !exists(&launcher_jar).await {
                        return None;
                    }
                    let config_file = match std::env::consts::OS {
                        "macos" => "config_mac",
                        "windows" => "config_win",
                        _ => "config_linux",
                    };
                    let data_dir = std::env::temp_dir()
                        .join(format!("opencode-jdtls-data-{}", rand::random::<u64>()));
                    std::fs::create_dir_all(&data_dir).ok()?;
                    let launcher = launcher_jar.to_string_lossy().into_owned();
                    let configuration = dist_path.join(config_file).to_string_lossy().into_owned();
                    let data = data_dir.to_string_lossy().into_owned();
                    spawn_bin(
                        &java,
                        &[
                            "-jar".to_string(),
                            launcher,
                            "-configuration".to_string(),
                            configuration,
                            "-data".to_string(),
                            data,
                            "-Declipse.application=org.eclipse.jdt.ls.core.id1".to_string(),
                            "-Dosgi.bundles.defaultStartLevel=4".to_string(),
                            "-Declipse.product=org.eclipse.jdt.ls.core.product".to_string(),
                            "-Dlog.level=ALL".to_string(),
                            "--add-modules=ALL-SYSTEM".to_string(),
                            "--add-opens java.base/java.util=ALL-UNNAMED".to_string(),
                            "--add-opens java.base/java.lang=ALL-UNNAMED".to_string(),
                        ],
                        &root,
                    )
                })
            }),
        ),
        server(
            "kotlin-ls",
            &[".kt", ".kts"],
            // The TS conditions after the first `NearestRoot` are dead code —
            // `NearestRoot` always returns the ctx.directory fallback.
            nearest_root(&["settings.gradle.kts", "settings.gradle"], None),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    let launcher_script = ctx.bin.join("kotlin-ls").join("kotlin-lsp.sh");
                    if !exists(&launcher_script).await {
                        return None;
                    }
                    spawn_bin(&launcher_script, &["--stdio".to_string()], &root)
                })
            }),
        ),
        server(
            "yaml-ls",
            &[".yaml", ".yml"],
            nearest_root(
                &[
                    "package-lock.json",
                    "bun.lockb",
                    "bun.lock",
                    "pnpm-lock.yaml",
                    "yarn.lock",
                ],
                None,
            ),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    let bin = match which("yaml-language-server", &ctx) {
                        Some(bin) => bin,
                        None => npm_which("yaml-language-server", None, &ctx)?,
                    };
                    spawn_with(&bin, &["--stdio".to_string()], &root, None, None)
                })
            }),
        ),
        server(
            "lua-ls",
            &[".lua"],
            nearest_root(
                &[
                    ".luarc.json",
                    ".luarc.jsonc",
                    ".luacheckrc",
                    ".stylua.toml",
                    "stylua.toml",
                    "selene.toml",
                    "selene.yml",
                ],
                None,
            ),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    let bin = which("lua-language-server", &ctx)?;
                    spawn_bin(&bin, &[], &root)
                })
            }),
        ),
        server(
            "php intelephense",
            &[".php"],
            nearest_root(&["composer.json", "composer.lock", ".php-version"], None),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    let bin = match which("intelephense", &ctx) {
                        Some(bin) => bin,
                        None => npm_which("intelephense", None, &ctx)?,
                    };
                    let initialization = serde_json::json!({ "telemetry": { "enabled": false } });
                    spawn_with(
                        &bin,
                        &["--stdio".to_string()],
                        &root,
                        None,
                        Some(initialization.as_object().expect("object").clone()),
                    )
                })
            }),
        ),
        server(
            "prisma",
            &[".prisma"],
            nearest_root(
                &["schema.prisma", "prisma/schema.prisma", "prisma"],
                Some(&["package.json"]),
            ),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    let bin = which("prisma", &ctx)?;
                    spawn_bin(&bin, &["language-server".to_string()], &root)
                })
            }),
        ),
        server(
            "dart",
            &[".dart"],
            nearest_root(&["pubspec.yaml", "analysis_options.yaml"], None),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    let bin = which("dart", &ctx)?;
                    spawn_bin(
                        &bin,
                        &["language-server".to_string(), "--lsp".to_string()],
                        &root,
                    )
                })
            }),
        ),
        server(
            "ocaml-lsp",
            &[".ml", ".mli"],
            nearest_root(&["dune-project", "dune-workspace", ".merlin", "opam"], None),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    let bin = which("ocamllsp", &ctx)?;
                    spawn_bin(&bin, &[], &root)
                })
            }),
        ),
        server(
            "bash",
            &[".sh", ".bash", ".zsh", ".ksh"],
            root_fn(|_file, ctx| Box::pin(async move { Some(ctx.directory.clone()) })),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    let bin = match which("bash-language-server", &ctx) {
                        Some(bin) => bin,
                        None => npm_which("bash-language-server", None, &ctx)?,
                    };
                    spawn_with(&bin, &["start".to_string()], &root, None, None)
                })
            }),
        ),
        server(
            "terraform",
            &[".tf", ".tfvars"],
            nearest_root(&[".terraform.lock.hcl", "terraform.tfstate", "*.tf"], None),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    let bin = which("terraform-ls", &ctx)?;
                    let initialization = serde_json::json!({
                        "experimentalFeatures": {
                            "prefillRequiredFields": true,
                            "validateOnSave": true,
                        }
                    });
                    let mut handle = spawn_bin(&bin, &["serve".to_string()], &root)?;
                    handle.initialization =
                        Some(initialization.as_object().expect("object").clone());
                    Some(handle)
                })
            }),
        ),
        server(
            "texlab",
            &[".tex", ".bib"],
            nearest_root(
                &[".latexmkrc", "latexmkrc", ".texlabroot", "texlabroot"],
                None,
            ),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    let bin = which("texlab", &ctx)?;
                    spawn_bin(&bin, &[], &root)
                })
            }),
        ),
        server(
            "dockerfile",
            &[".dockerfile", "Dockerfile"],
            root_fn(|_file, ctx| Box::pin(async move { Some(ctx.directory.clone()) })),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    let bin = match which("docker-langserver", &ctx) {
                        Some(bin) => bin,
                        None => npm_which("dockerfile-language-server-nodejs", None, &ctx)?,
                    };
                    spawn_with(&bin, &["--stdio".to_string()], &root, None, None)
                })
            }),
        ),
        server(
            "gleam",
            &[".gleam"],
            nearest_root(&["gleam.toml"], None),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    let bin = which("gleam", &ctx)?;
                    spawn_bin(&bin, &["lsp".to_string()], &root)
                })
            }),
        ),
        server(
            "clojure-lsp",
            &[".clj", ".cljs", ".cljc", ".edn"],
            nearest_root(
                &[
                    "deps.edn",
                    "project.clj",
                    "shadow-cljs.edn",
                    "bb.edn",
                    "build.boot",
                ],
                None,
            ),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    let bin = which("clojure-lsp", &ctx)?;
                    spawn_bin(&bin, &["listen".to_string()], &root)
                })
            }),
        ),
        server(
            "nixd",
            &[".nix"],
            root_fn(|file, ctx| {
                let file = file.to_string();
                Box::pin(async move {
                    let flake_root = nearest_root(&["flake.nix"], None)(&file, ctx.clone()).await;
                    if let Some(root) = flake_root {
                        if root != ctx.directory {
                            return Some(root);
                        }
                    }
                    if ctx.worktree != ctx.directory {
                        return Some(ctx.worktree.clone());
                    }
                    Some(ctx.directory.clone())
                })
            }),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    let bin = which("nixd", &ctx)?;
                    spawn_with(&bin, &[], &root, None, None)
                })
            }),
        ),
        server(
            "tinymist",
            &[".typ", ".typc"],
            nearest_root(&["typst.toml"], None),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    let bin = which("tinymist", &ctx)?;
                    spawn_bin(&bin, &[], &root)
                })
            }),
        ),
        server(
            "haskell-language-server",
            &[".hs", ".lhs"],
            nearest_root(
                &["stack.yaml", "cabal.project", "hie.yaml", "*.cabal"],
                None,
            ),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    let bin = which("haskell-language-server-wrapper", &ctx)?;
                    spawn_bin(&bin, &["--lsp".to_string()], &root)
                })
            }),
        ),
        server(
            "julials",
            &[".jl"],
            nearest_root(&["Project.toml", "Manifest.toml", "*.jl"], None),
            spawn_fn(|root, ctx| {
                Box::pin(async move {
                    let bin = which("julia", &ctx)?;
                    spawn_bin(
                        &bin,
                        &[
                            "--startup-file=no".to_string(),
                            "--history-file=no".to_string(),
                            "-e".to_string(),
                            "using LanguageServer; runserver()".to_string(),
                        ],
                        &root,
                    )
                })
            }),
        ),
    ]
}

/// `findUp` (util/filesystem.ts:182-211) — every match walking up from
/// `start`, excluding `stop` itself.
async fn find_up_all(target: &str, start: &Path, stop: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![start.to_path_buf()];
    let mut current = start.to_path_buf();
    loop {
        if current == stop {
            break;
        }
        let Some(parent) = current.parent() else {
            break;
        };
        let parent = parent.to_path_buf();
        if parent == current {
            break;
        }
        dirs.push(parent.clone());
        current = parent;
    }
    let mut result = Vec::new();
    for dir in dirs {
        let search = dir.join(target);
        if exists(&search).await {
            result.push(search);
        }
    }
    result
}

/// `isModuleOf` (server.ts:1126-1242).
fn is_module_of(pom_content: &str, module_path: &str) -> bool {
    let normalized = module_path.replace('\\', "/");
    let normalized = normalized.strip_suffix('/').unwrap_or(&normalized);
    if normalized.is_empty() {
        return false;
    }
    let modules = extract_module_blocks(pom_content);
    for block in modules {
        for declaration in module_declarations(&block) {
            let mut decl = declaration.replace('\\', "/");
            if let Some(stripped) = decl.strip_prefix("./") {
                decl = stripped.to_string();
            }
            let decl = decl.strip_suffix('/').unwrap_or(&decl).to_string();
            if decl == *normalized {
                return true;
            }
        }
    }
    false
}

fn extract_module_blocks(pom_content: &str) -> Vec<String> {
    static MODULES: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let regex = MODULES.get_or_init(|| {
        regex::Regex::new(r"<modules>([\s\S]*?)</modules>").expect("modules regex")
    });
    regex
        .find_iter(pom_content)
        .map(|m| m.as_str().to_string())
        .collect()
}

fn module_declarations(block: &str) -> Vec<String> {
    static MODULE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let regex = MODULE.get_or_init(|| {
        regex::Regex::new(r"<module>\s*([^<]+?)\s*</module>").expect("module regex")
    });
    let stripped = strip_xml_comments(block);
    regex
        .captures_iter(&stripped)
        .map(|captures| captures[1].to_string())
        .collect()
}

fn strip_xml_comments(input: &str) -> String {
    static COMMENTS: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let regex =
        COMMENTS.get_or_init(|| regex::Regex::new(r"<!--[\s\S]*?-->").expect("comment regex"));
    regex.replace_all(input, "").into_owned()
}

fn java_major_version(output: &str) -> Option<u32> {
    let re = regex::Regex::new(r#""(\d+)\.\d+\.\d+""#).ok()?;
    let captures = re.captures(output)?;
    captures[1].parse().ok()
}

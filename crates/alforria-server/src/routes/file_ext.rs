//! File tickets, raw downloads and uploads (alforria extension).
//!
//! The frozen file routes return JSON and take an `Authorization` header,
//! which is no use to a browser that wants to show a workspace image in an
//! `<img>` or offer a download link, and they have no way to put a file
//! *into* the workspace. These routes sit outside the frozen v1/v2 tables:
//!
//! - `POST /file/ticket`           a short-lived read ticket for one file
//! - `GET  /file/raw?path=`        the file's bytes, with its media type
//! - `PUT  /file/content?path=`    write a file (the request body, as is)
//!
//! and the frozen `GET /file/content?path=` accepts a ticket too. Like the
//! workflow routes they take `?directory=` (or `x-opencode-directory`).
//!
//! Every path is resolved against the project directory the way the kernel
//! resolves it (`alforria_core::protect::resolve_path`: links read where
//! they stand, `..` applied to where the path has got to), and must land
//! inside the project; a path that cannot be resolved is refused — the
//! boundary the tools draw with `external_directory`
//! (`alforria_core::tool::external_directory`). A tool can ask the user to
//! cross it; an HTTP request cannot, so here it is refused.

use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Extension, Json, Router};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::bearer::{Principal, Via};
use crate::error::{ApiError, ServerError};
use crate::middleware::auth::query_param;
use crate::middleware::directory::RequestDirectory;
use crate::middleware::location::LocationContext;
use crate::state::ServerContext;
use crate::ticket::{FileScope, FILE_TICKET_MAX_TTL, FILE_TICKET_TTL};

pub fn register(router: Router<Arc<ServerContext>>) -> Router<Arc<ServerContext>> {
    router
        .route("/file/ticket", post(ticket))
        .route("/file/raw", get(raw))
        // `GET /file/content` is the frozen JSON read; axum merges the PUT
        // onto the same path.
        .route("/file/content", put(upload))
}

/// A refusal from these routes: an existing server error, or a status with
/// the `{ name, data: { message } }` body the v1 surface uses.
pub(crate) enum FileError {
    Server(ServerError),
    Status(StatusCode, &'static str, String),
}

impl From<ServerError> for FileError {
    fn from(err: ServerError) -> FileError {
        FileError::Server(err)
    }
}

impl IntoResponse for FileError {
    fn into_response(self) -> Response {
        match self {
            FileError::Server(err) => err.into_response(),
            FileError::Status(status, name, message) => {
                let mut response =
                    Json(json!({ "name": name, "data": { "message": message } })).into_response();
                *response.status_mut() = status;
                response
            }
        }
    }
}

fn bad_request(message: impl Into<String>) -> FileError {
    FileError::Status(StatusCode::BAD_REQUEST, "BadRequest", message.into())
}

fn forbidden(message: impl Into<String>) -> FileError {
    FileError::Server(ServerError::Api(ApiError::Forbidden {
        message: message.into(),
    }))
}

fn not_found(message: impl Into<String>) -> FileError {
    FileError::Status(StatusCode::NOT_FOUND, "NotFoundError", message.into())
}

fn too_large(limit: u64) -> FileError {
    FileError::Status(
        StatusCode::PAYLOAD_TOO_LARGE,
        "PayloadTooLargeError",
        format!("the upload is larger than the {limit}-byte limit (server.upload.max_bytes)"),
    )
}

fn over_quota(message: String) -> FileError {
    FileError::Status(StatusCode::PAYLOAD_TOO_LARGE, "UploadQuotaError", message)
}

/// The name prefix of an upload still being received. A credential below
/// `admin` may not upload to a path with a component that starts with it
/// ([`restricted_upload_allowed`]), and nothing is left out of the count
/// for carrying it.
const TEMPORARY_PREFIX: &str = ".alforria-upload-";

/// How deep below its upload directory a credential below `admin` may
/// upload, in path components, and how deep the count goes.
pub const UPLOAD_MAX_DEPTH: usize = 8;

/// How long a directory's usage is trusted before it is counted again:
/// files the agent's shell adds or removes are noticed after this long, or
/// on restart.
const USAGE_MAX_AGE: Duration = Duration::from_secs(300);

/// How long after a count that an upload overtook the next one is tried.
const USAGE_RETRY: Duration = Duration::from_secs(10);

/// What an upload directory holds, as the server last counted it and has
/// kept up to date since with every upload it wrote.
#[derive(Clone, Copy)]
struct Usage {
    bytes: u64,
    /// Files, links and directories alike.
    entries: u64,
    /// The count did not finish — too many entries, or too deep: the
    /// directory is treated as full.
    over: bool,
    counted: std::time::Instant,
}

/// One upload directory's count and the lock every change to it is made
/// under.
#[derive(Default)]
struct Area {
    state: std::sync::Mutex<AreaState>,
}

#[derive(Default)]
struct AreaState {
    /// `None` until the directory has been counted.
    usage: Option<Usage>,
    /// Moves with every change the server makes to the count: a walk that
    /// one of them overtook is not written over it.
    changes: u64,
    walks: u64,
}

impl Area {
    /// Held for the one stat-check-rename-count that makes an upload count
    /// ([`commit`]) and for making an upload's directories — never across
    /// the body, a walk or an await.
    fn lock(&self) -> std::sync::MutexGuard<'_, AreaState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The upload directories this process has counted. The map's own lock is
/// held to look one up, nothing more: uploads into different directories
/// do not wait for each other.
static AREAS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<PathBuf, Arc<Area>>>> =
    std::sync::LazyLock::new(Default::default);

fn area_of(root: &Path) -> Arc<Area> {
    let mut areas = AREAS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    Arc::clone(areas.entry(root.to_path_buf()).or_default())
}

/// How many times `area` has been counted by walking it (for tests: an
/// upload does not walk).
#[doc(hidden)]
pub fn upload_area_walks(area: &Path) -> u64 {
    area_of(area).lock().walks
}

/// The running count of `area` — bytes, entries — when it has one (for
/// tests: it is what the directory holds).
#[doc(hidden)]
pub fn upload_area_usage(area: &Path) -> Option<(u64, u64)> {
    let usage = area_of(area).lock().usage?;
    Some((usage.bytes, usage.entries))
}

/// What `area` holds on disk, counted the way the quota counts it.
#[doc(hidden)]
pub fn upload_area_on_disk(area: &Path, max_entries: u64) -> (u64, u64) {
    let usage = count_area(area, max_entries);
    (usage.bytes, usage.entries)
}

/// Count what `root` holds: every entry, directories included, no link
/// followed, nothing skipped by name. Bounded: it gives up — `over` — past
/// `max_entries` entries or below [`UPLOAD_MAX_DEPTH`].
fn count_area(root: &Path, max_entries: u64) -> Usage {
    let mut usage = Usage {
        bytes: 0,
        entries: 0,
        over: false,
        counted: std::time::Instant::now(),
    };
    let mut pending = vec![(root.to_path_buf(), 0usize)];
    while let Some((directory, depth)) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            usage.entries += 1;
            if usage.entries > max_entries {
                usage.over = true;
                return usage;
            }
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                if depth + 1 >= UPLOAD_MAX_DEPTH {
                    usage.over = true;
                    return usage;
                }
                pending.push((entry.path(), depth + 1));
            } else {
                usage.bytes = usage.bytes.saturating_add(meta.len());
            }
        }
    }
    usage
}

/// Make sure `area` has a count no older than [`USAGE_MAX_AGE`]: the
/// directory is walked — outside the lock — when this process has not
/// counted it yet (so after every start) or last did too long ago, which is
/// what corrects the running count for files the agent's shell added or
/// removed. A walk that an upload overtook saw the directory half-way
/// through that upload: it is dropped and the running count stands until
/// the next one. A walk that hits its cap leaves the directory full.
async fn refresh(ctx: &ServerContext, root: &Path, area: &Arc<Area>) -> Result<(), FileError> {
    let before = {
        let state = area.lock();
        if state
            .usage
            .is_some_and(|usage| usage.counted.elapsed() < USAGE_MAX_AGE)
        {
            return Ok(());
        }
        state.changes
    };
    let max_entries = ctx.policy.upload_max_files;
    let walked = root.to_path_buf();
    let counted = tokio::task::spawn_blocking(move || count_area(&walked, max_entries))
        .await
        .map_err(|join| storage_error(&std::io::Error::other(join.to_string())))?;
    let mut state = area.lock();
    state.walks += 1;
    let overtaken = state.changes != before;
    match &mut state.usage {
        Some(usage) if overtaken => {
            // Not again on every upload of a busy directory.
            let now = std::time::Instant::now();
            usage.counted = now
                .checked_sub(USAGE_MAX_AGE.saturating_sub(USAGE_RETRY))
                .unwrap_or(now);
        }
        usage => *usage = Some(counted),
    }
    Ok(())
}

/// What an upload adds to its directory.
#[derive(Clone, Copy)]
struct Addition {
    /// The upload's size.
    bytes: u64,
    /// The size of the file it replaces.
    replaced: u64,
    /// The file, when new, and the directories still to be made for it.
    entries: u64,
}

/// Whether `usage` has room for `addition` (`server.upload.max_total_bytes`,
/// `server.upload.max_files`); the bytes left for the upload when it has.
/// A directory that has no count, or whose count did not finish, is full.
fn room(
    ctx: &ServerContext,
    area: &Path,
    usage: Option<&Usage>,
    addition: Addition,
) -> Result<u64, FileError> {
    let (max_bytes, max_entries) = (
        ctx.policy.upload_max_total_bytes,
        ctx.policy.upload_max_files,
    );
    let shown = area.display();
    let usage = usage.filter(|usage| !usage.over);
    let Some(usage) = usage.filter(|usage| usage.entries + addition.entries <= max_entries) else {
        return Err(over_quota(format!(
            "the upload directory {shown} may hold {max_entries} files and directories, \
             no more than {UPLOAD_MAX_DEPTH} deep, and is full (server.upload.max_files)"
        )));
    };
    let used = usage.bytes.saturating_sub(addition.replaced);
    let left = max_bytes.saturating_sub(used);
    if addition.bytes > left {
        return Err(over_quota(format!(
            "the upload does not fit: the upload directory {shown} holds {used} of the \
             {max_bytes} bytes it may (server.upload.max_total_bytes)"
        )));
    }
    Ok(left)
}

/// The start of an upload below `admin`, in one hold of the area's lock:
/// a first look at the quota — so that an upload that cannot fit is refused
/// before anything is made or received — then the target's directories,
/// which count from the moment they exist whatever becomes of the upload.
/// Returns the handle the upload is written through and the bytes it may
/// receive before it is cut off. Neither is the decision: that is
/// [`commit`]'s, on what is there then.
fn begin(
    ctx: &ServerContext,
    root: &Path,
    area: &Area,
    target: &Path,
    declared: u64,
) -> Result<std::io::Result<(alforria_core::beneath::NoFollowDir, u64)>, FileError> {
    let mut state = area.lock();
    let missing = target
        .ancestors()
        .skip(1)
        .take_while(|path| path.starts_with(root) && *path != root)
        .filter(|path| std::fs::symlink_metadata(path).is_err())
        .count() as u64;
    let existing = std::fs::symlink_metadata(target).ok();
    let addition = Addition {
        bytes: declared,
        replaced: existing.as_ref().map_or(0, std::fs::Metadata::len),
        entries: missing + u64::from(existing.is_none()),
    };
    let left = room(ctx, root, state.usage.as_ref(), addition)?;
    let directory = alforria_core::protect::open_parent_of(target);
    if missing > 0 {
        // Made or not, in whole or in part: counted, and corrected by the
        // next walk if that was too many.
        if let Some(usage) = &mut state.usage {
            usage.entries += missing;
        }
        state.changes += 1;
    }
    Ok(directory.map(|directory| (directory, left)))
}

/// Make a received upload the file `name`, and count it — one hold of the
/// area's lock around all of it. What it replaces is read here, through
/// the handle the rename goes through, and the same reading decides
/// whether there is room and is taken off the count: two uploads replacing
/// one file cannot both be credited with it.
fn commit(
    ctx: &ServerContext,
    root: &Path,
    area: &Area,
    directory: &alforria_core::beneath::NoFollowDir,
    temporary: &std::ffi::OsStr,
    name: &std::ffi::OsStr,
    written: u64,
) -> Result<std::io::Result<()>, FileError> {
    let mut state = area.lock();
    let replaced = match directory.len_of(name) {
        Ok(replaced) => replaced,
        Err(err) => return Ok(Err(err)),
    };
    let addition = Addition {
        bytes: written,
        replaced: replaced.unwrap_or(0),
        entries: u64::from(replaced.is_none()),
    };
    room(ctx, root, state.usage.as_ref(), addition)?;
    if let Err(err) = directory.rename(temporary, name) {
        return Ok(Err(err));
    }
    state.changes += 1;
    if let Some(usage) = &mut state.usage {
        usage.bytes = usage
            .bytes
            .saturating_sub(addition.replaced)
            .saturating_add(written);
        usage.entries += addition.entries;
    }
    Ok(Ok(()))
}

/// The project a request addresses: the directory the auth layer resolved
/// and the allowlist admitted (`middleware::directory`).
fn location(
    ctx: &ServerContext,
    directory: &RequestDirectory,
) -> Result<LocationContext, ServerError> {
    let directory = directory.0.clone();
    let services = ctx.instances.load(&directory)?;
    Ok(LocationContext {
        directory,
        workspace_id: None,
        services,
    })
}

/// Resolve a request's `path` to the file it denotes inside the project:
/// relative paths are relative to the project directory, the path is
/// resolved as the kernel would resolve that spelling
/// (`protect::resolve_path`), and the result must be inside the project.
/// Everything after this acts on the resolved path, never on the spelling.
pub(crate) fn project_file(location: &LocationContext, path: &str) -> Result<PathBuf, FileError> {
    let joined = spelled(location, path)?;
    // Unresolvable (a `..` after something that does not exist, a link
    // loop): no telling where it leads, so it is not inside.
    let resolved = crate::confine::resolve_for_access(&joined).map_err(|_| outside(path))?;
    inside_project(location, path, resolved)
}

fn outside(path: &str) -> FileError {
    forbidden(format!("{path} is outside the project directory"))
}

/// A request's `path` as the kernel would be handed it: absolute as given,
/// otherwise below the project directory.
fn spelled(location: &LocationContext, path: &str) -> Result<PathBuf, FileError> {
    if path.is_empty() {
        return Err(bad_request("path is required"));
    }
    if path.contains('\0') {
        return Err(bad_request("path is not a valid file name"));
    }
    Ok(if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        location.directory.join(path)
    })
}

fn inside_project(
    location: &LocationContext,
    path: &str,
    resolved: PathBuf,
) -> Result<PathBuf, FileError> {
    if !crate::confine::project_roots(location)
        .iter()
        .any(|root| alforria_core::protect::within(&resolved, root))
    {
        return Err(outside(path));
    }
    Ok(resolved)
}

/// Where an upload to `path` lands. As [`project_file`], with the one
/// difference the write itself makes: the upload renames a temporary file
/// over its target, and a rename replaces the last component, it does not
/// follow it. So a last component that is a link to nothing — which a
/// write by name would create a file through, wherever it points — lands
/// where the link stands: the resolved parent plus the name. (A link to
/// something that exists is judged where it leads, and the file it leads to
/// is what gets replaced.)
fn upload_target(location: &LocationContext, path: &str) -> Result<PathBuf, FileError> {
    let joined = spelled(location, path)?;
    let dangling = std::fs::symlink_metadata(&joined).is_ok_and(|meta| meta.is_symlink())
        && std::fs::metadata(&joined).is_err();
    match (dangling, joined.parent(), joined.file_name()) {
        (true, Some(parent), Some(name)) => {
            let parent = crate::confine::resolve_for_access(parent).map_err(|_| outside(path))?;
            inside_project(location, path, parent.join(name))
        }
        _ => project_file(location, path),
    }
}

fn required_path(uri: &Uri) -> Result<String, FileError> {
    query_param(uri.query(), "path")
        .filter(|path| !path.is_empty())
        .ok_or_else(|| bad_request("the path query parameter is required"))
}

/// The scope a ticket for `file` in `location` carries.
pub(crate) fn file_scope(location: &LocationContext, file: &Path) -> FileScope {
    FileScope {
        // An identity, not a confinement decision: the same function gives
        // the same key when the ticket is minted, when the auth layer
        // matches it and here. The file is the resolved one.
        directory: crate::state::resolve_directory(&location.directory),
        file: file.to_path_buf(),
    }
}

/// A request that got past authentication on a ticket alone may read the
/// ticket's file and nothing else. The auth layer already matched the
/// ticket to the request; this is the same check at the point of use,
/// against the file the handler is about to open.
pub(crate) fn check_ticket(
    ctx: &ServerContext,
    principal: &Option<Extension<Principal>>,
    uri: &Uri,
    location: &LocationContext,
    file: &Path,
) -> Result<(), FileError> {
    let ticketed = principal
        .as_ref()
        .is_some_and(|principal| principal.0.via == Via::Ticket);
    if !ticketed {
        return Ok(());
    }
    let ticket = query_param(uri.query(), "ticket").unwrap_or_default();
    if ctx
        .file_tickets
        .admits(&ticket, &file_scope(location, file))
    {
        Ok(())
    } else {
        Err(forbidden("this ticket is for a different file"))
    }
}

#[derive(Deserialize)]
struct TicketRequest {
    path: String,
    /// Seconds; default 60, at most 600.
    #[serde(default)]
    ttl: Option<u64>,
}

/// `POST /file/ticket` — mint a read ticket for one existing file.
async fn ticket(
    State(ctx): State<Arc<ServerContext>>,
    axum::Extension(directory): axum::Extension<RequestDirectory>,
    Json(request): Json<TicketRequest>,
) -> Result<Json<serde_json::Value>, FileError> {
    let location = location(&ctx, &directory)?;
    let file = project_file(&location, &request.path)?;
    match std::fs::metadata(&file) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => return Err(bad_request(format!("{} is not a file", request.path))),
        Err(_) => return Err(not_found(format!("{} does not exist", request.path))),
    }
    let ttl = request
        .ttl
        .map(|seconds| Duration::from_secs(seconds.max(1)).min(FILE_TICKET_MAX_TTL))
        .unwrap_or(FILE_TICKET_TTL);
    let ticket = ctx.file_tickets.issue(file_scope(&location, &file), ttl);
    let encode = |value: &str| -> String {
        value
            .bytes()
            .map(|byte| match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                    (byte as char).to_string()
                }
                other => format!("%{other:02X}"),
            })
            .collect()
    };
    let query = format!(
        "path={}&directory={}&ticket={ticket}",
        encode(&request.path),
        encode(&location.directory.display().to_string()),
    );
    Ok(Json(json!({
        "ticket": ticket,
        "expires_in": ttl.as_secs(),
        "path": request.path,
        // Ready to append to the server's origin.
        "url": format!("/file/raw?{query}"),
    })))
}

/// The media type to serve a file with. Anything not recognised is sniffed:
/// text when the head of the file has no NUL byte, opaque bytes otherwise.
fn media_type(file: &Path, head: &[u8]) -> String {
    let known = crate::routes::v1::global_control::mime_type(file);
    if !known.is_empty() {
        return known;
    }
    let extension = file
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "md" | "markdown" => "text/markdown; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "xml" => "application/xml",
        "avif" => "image/avif",
        "ogg" => "audio/ogg",
        "m4a" => "audio/mp4",
        "mov" => "video/quicktime",
        _ if head.contains(&0) => "application/octet-stream",
        _ => "text/plain; charset=utf-8",
    }
    .to_string()
}

/// `bytes=a-b`, `bytes=a-` or `bytes=-n` against a file of `len` bytes.
/// `None` for anything else (several ranges, another unit): the whole file
/// is served, which RFC 9110 allows.
fn parse_range(value: &str, len: u64) -> Option<Result<(u64, u64), ()>> {
    let spec = value.trim().strip_prefix("bytes=")?;
    if spec.contains(',') {
        return None;
    }
    let (start, end) = spec.split_once('-')?;
    let range = match (start.trim(), end.trim()) {
        ("", suffix) => {
            let suffix: u64 = suffix.parse().ok()?;
            if suffix == 0 || len == 0 {
                return Some(Err(()));
            }
            (len.saturating_sub(suffix), len - 1)
        }
        (start, "") => (start.parse().ok()?, len.saturating_sub(1)),
        (start, end) => (
            start.parse().ok()?,
            end.parse::<u64>().ok()?.min(len.saturating_sub(1)),
        ),
    };
    if len == 0 || range.0 > range.1 || range.0 >= len {
        return Some(Err(()));
    }
    Some(Ok(range))
}

fn disposition(file: &Path, download: bool) -> HeaderValue {
    let name: String = file
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".to_string())
        .chars()
        .map(|c| {
            if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let kind = if download { "attachment" } else { "inline" };
    HeaderValue::from_str(&format!("{kind}; filename=\"{name}\""))
        .unwrap_or_else(|_| HeaderValue::from_static("attachment"))
}

/// `GET /file/raw?path=[&download=1]` — the file's bytes.
///
/// The response can be embedded or downloaded but never *runs*: it is
/// served `nosniff` under a `sandbox` content security policy, so an HTML
/// or SVG file the agent wrote is displayed as an inert document rather
/// than as a page of this origin (where a browser may hold the operator's
/// Basic credentials).
async fn raw(
    State(ctx): State<Arc<ServerContext>>,
    principal: Option<Extension<Principal>>,
    uri: Uri,
    axum::Extension(directory): axum::Extension<RequestDirectory>,
    headers: HeaderMap,
) -> Result<Response, FileError> {
    let location = location(&ctx, &directory)?;
    let file = project_file(&location, &required_path(&uri)?)?;
    check_ticket(&ctx, &principal, &uri, &location, &file)?;
    let metadata = tokio::fs::metadata(&file)
        .await
        .map_err(|_| not_found("no such file"))?;
    if !metadata.is_file() {
        return Err(bad_request("not a file"));
    }
    let len = metadata.len();
    let mut handle = tokio::fs::File::open(&file)
        .await
        .map_err(|_| not_found("no such file"))?;
    let mut head = vec![0u8; 8192.min(len as usize)];
    let read = handle.read(&mut head).await.unwrap_or(0);
    head.truncate(read);
    let media_type = media_type(&file, &head);

    let range = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| parse_range(value, len));
    let download = query_param(uri.query(), "download").is_some_and(|value| value != "0");
    let mut response = Response::builder()
        .header(header::CONTENT_TYPE, media_type)
        .header(header::CONTENT_DISPOSITION, disposition(&file, download))
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CACHE_CONTROL, "private, no-store")
        .header("x-content-type-options", "nosniff")
        .header(
            "content-security-policy",
            "sandbox; default-src 'none'; img-src 'self' data:; media-src 'self'; style-src 'unsafe-inline'",
        );
    let (start, end) = match range {
        Some(Err(())) => {
            return Ok(Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header(header::CONTENT_RANGE, format!("bytes */{len}"))
                .body(Body::empty())
                .expect("static response parts are valid"));
        }
        Some(Ok((start, end))) => {
            response = response
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}"));
            (start, end)
        }
        None => (0, len.saturating_sub(1)),
    };
    let count = if len == 0 { 0 } else { end - start + 1 };
    handle
        .seek(SeekFrom::Start(start))
        .await
        .map_err(|err| storage_error(&err))?;
    let stream = tokio_util::io::ReaderStream::new(handle.take(count));
    Ok(response
        .header(header::CONTENT_LENGTH, count)
        .body(Body::from_stream(stream))
        .expect("response parts are valid"))
}

fn storage_error(err: &std::io::Error) -> FileError {
    FileError::Server(ServerError::Core(alforria_core::CoreError::Storage(
        format!("file access failed: {err}"),
    )))
}

/// Where a credential below `admin` may write `target` (already resolved,
/// inside the project).
///
/// An upload is a write nobody asked the model for, so it must not be a way
/// to change what the agent is told or allowed, or to run code. Two rules,
/// both on the resolved path:
///
/// * it must be inside one of the upload directories
///   (`server.upload.directories`, default `uploads/`) — an allowlist,
///   because "everywhere except the files that matter" is a list that
///   falls behind;
/// * no component of it may be a name the loaders act on
///   (`alforria_core::project_files`: config files, `.opencode`, `.claude`,
///   `.agents`, instruction files, `.git`), wherever it sits — which is
///   what keeps `"."` as an upload directory from being a way in.
///
/// Returns the upload directory the target is in, which is what the quota
/// is counted over ([`quota_room`]).
fn restricted_upload_allowed(
    ctx: &ServerContext,
    location: &LocationContext,
    target: &Path,
) -> Result<PathBuf, String> {
    use alforria_core::protect::{resolve, resolve_path, within};
    // Where the project is for the kernel, like the target (`project_file`).
    let project = resolve(&location.directory);
    let upload_area = ctx.policy.upload_directories.iter().find_map(|directory| {
        // The area as the kernel reaches it: a configured `uploads/../x`
        // or a link is where it leads. One that cannot be resolved holds
        // nothing. The area itself must be inside the project too.
        resolve_path(&project.join(directory))
            .ok()
            .filter(|root| within(root, &project) && within(target, root))
    });
    // The area is a directory of its own: one that is the project itself
    // (a link to it, say) holds nothing.
    let upload_area = upload_area.filter(|root| *root != project);
    let Some(upload_area) = upload_area else {
        return Err(format!(
            "this token may only upload into {} (server.upload.directories)",
            ctx.policy.upload_directories.join(", ")
        ));
    };
    let below: Vec<String> = target
        .strip_prefix(&upload_area)
        .map(|relative| {
            relative
                .components()
                .map(|part| part.as_os_str().to_string_lossy().to_lowercase())
                .collect()
        })
        .unwrap_or_default();
    if below.len() > UPLOAD_MAX_DEPTH {
        return Err(format!(
            "{} is more than {UPLOAD_MAX_DEPTH} levels below the upload directory",
            target.display()
        ));
    }
    // The server's own name for an upload in progress is not a client's.
    if below.iter().any(|part| part.starts_with(TEMPORARY_PREFIX)) {
        return Err(format!(
            "{} uses a name reserved for uploads in progress ({TEMPORARY_PREFIX}*)",
            target.display()
        ));
    }
    // The same guard the edit tools' writes go through
    // (`alforria_core::protect`): loader names anywhere on the resolved
    // path, and every location the loaders consult for this instance.
    // (A target the guard cannot judge — a link as the last component — is
    // this route's own business: it has been resolved and confined to the
    // upload area above, and the upload replaces a link by renaming over
    // it instead of writing through it.)
    let refusal = location
        .services
        .agents
        .protected()
        .guards(target)
        .filter(|refusal| refusal.landing.is_some());
    if let Some(refusal) = refusal {
        let reason = refusal.reason;
        return Err(format!(
            "{reason}: this token may not write config, instruction, agent, command, skill, \
             workflow or git files"
        ));
    }
    // And the names a formatter or a language server is configured or
    // started from, below the project (`project_files::TOOL_CONFIG_FILES`):
    // the write guard leaves those to the agent, an upload has no business
    // with them.
    if target
        .strip_prefix(&project)
        .is_ok_and(alforria_core::project_files::is_tool_config_path)
    {
        return Err(format!(
            "{} is a file a formatter or language server is configured or started from: \
             this token may not upload it",
            target.display()
        ));
    }
    Ok(upload_area)
}

/// `PUT /file/content?path=[&overwrite=false]` — write the request body to
/// a file in the project, creating its directories.
///
/// Below `admin` the target is further limited to the upload area
/// ([`restricted_upload_allowed`]).
///
/// The body is the file, byte for byte (no multipart, no JSON envelope),
/// and is streamed to a temporary file beside the target, which replaces
/// it in one rename: a failed or oversized upload leaves the old file as it
/// was.
async fn upload(
    State(ctx): State<Arc<ServerContext>>,
    principal: Option<Extension<Principal>>,
    uri: Uri,
    axum::Extension(directory): axum::Extension<RequestDirectory>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, FileError> {
    let limit = ctx.policy.upload_max_bytes;
    let declared = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    if declared.is_some_and(|declared| declared > limit) {
        return Err(too_large(limit));
    }
    let location = location(&ctx, &directory)?;
    let path = required_path(&uri)?;
    let target = upload_target(&location, &path)?;
    if target.is_dir() {
        return Err(bad_request(format!("{path} is a directory")));
    }
    let area = match crate::confine::restricted(&principal) {
        true => {
            let root = restricted_upload_allowed(&ctx, &location, &target).map_err(forbidden)?;
            let area = area_of(&root);
            Some((root, area))
        }
        false => None,
    };
    let existed = target.exists();
    let overwrite = query_param(uri.query(), "overwrite").is_none_or(|value| value != "false");
    if existed && !overwrite {
        return Err(FileError::Status(
            StatusCode::CONFLICT,
            "FileExistsError",
            format!("{path} already exists"),
        ));
    }
    let io = |err: std::io::Error| storage_error(&err);
    let name = target
        .file_name()
        .ok_or_else(|| bad_request("path has no file name"))?
        .to_os_string();
    // The target has been resolved and judged; from here on it is not
    // looked up by name again. Its parent is opened — and created — one
    // component at a time with no link followed, and the temporary file
    // and the rename go through that handle
    // (`alforria_core::protect::open_parent_of`). A link put in the way
    // after the judgement is a refusal, not a different destination.
    //
    // Below `admin`, the quota of the upload directory comes first: a body
    // that declares a length that cannot fit is refused before anything is
    // written, one that does not is cut off at what is left. From the
    // server's running count — an upload does not walk the directory.
    let opened = match &area {
        Some((root, area)) => {
            refresh(&ctx, root, area).await?;
            let (ctx, root, area) = (Arc::clone(&ctx), root.clone(), Arc::clone(area));
            let (target, declared) = (target.clone(), declared.unwrap_or(0));
            tokio::task::spawn_blocking(move || begin(&ctx, &root, &area, &target, declared))
                .await
                .unwrap_or_else(|join| Ok(Err(std::io::Error::other(join.to_string()))))?
                .map(|(directory, left)| (directory, Some(left)))
        }
        None => {
            let target = target.clone();
            tokio::task::spawn_blocking(move || alforria_core::protect::open_parent_of(&target))
                .await
                .unwrap_or_else(|join| Err(std::io::Error::other(join.to_string())))
                .map(|directory| (directory, None))
        }
    };
    let (directory, room_left) = opened.map_err(|err| match err.kind() {
        std::io::ErrorKind::PermissionDenied => {
            forbidden(format!("{path} changed while it was being written: {err}"))
        }
        _ => io(err),
    })?;
    let temporary = std::ffi::OsString::from(format!(
        "{TEMPORARY_PREFIX}{}",
        uuid::Uuid::new_v4().simple()
    ));
    let mut file = tokio::fs::File::from_std(directory.create(&temporary, true).map_err(io)?);
    let mut written: u64 = 0;
    let mut stream = body.into_data_stream();
    let outcome: Result<(), Option<std::io::Error>> = async {
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|err| Some(std::io::Error::other(err.to_string())))?;
            written += chunk.len() as u64;
            if written > limit || room_left.is_some_and(|left| written > left) {
                return Err(None);
            }
            file.write_all(&chunk).await.map_err(Some)?;
        }
        file.flush().await.map_err(Some)?;
        file.sync_all().await.map_err(Some)
    }
    .await;
    drop(file);
    match outcome {
        Ok(()) => {}
        Err(cause) => {
            let _ = directory.remove_file(&temporary);
            return Err(match cause {
                None if written > limit => too_large(limit),
                None => over_quota(format!(
                    "the upload does not fit in what is left of the upload directory's \
                     {} bytes (server.upload.max_total_bytes)",
                    ctx.policy.upload_max_total_bytes
                )),
                Some(err) => io(err),
            });
        }
    }
    // Decided now, on what was received and what is there: see `commit`.
    let renamed = match &area {
        Some((root, area)) => {
            match commit(&ctx, root, area, &directory, &temporary, &name, written) {
                Ok(renamed) => renamed,
                Err(refusal) => {
                    let _ = directory.remove_file(&temporary);
                    return Err(refusal);
                }
            }
        }
        None => directory.rename(&temporary, &name),
    };
    if let Err(err) = renamed {
        let _ = directory.remove_file(&temporary);
        return Err(io(err));
    }
    // Clients watching the project learn of the file like any other edit.
    let _ = location.services.events.publish(
        &alforria_core::tool::edit::FILE_EDITED,
        json!({ "file": target.display().to_string() }),
        alforria_core::event::bus::PublishOptions::default(),
    );
    // Display only: the path the response names, relative when it can be.
    let relative = target
        .strip_prefix(alforria_core::protect::resolve(&location.directory))
        .map(|relative| relative.display().to_string())
        .unwrap_or_else(|_| target.display().to_string());
    let mut response = Json(json!({
        "path": relative,
        "absolute": target.display().to_string(),
        "size": written,
        "created": !existed,
    }))
    .into_response();
    if !existed {
        *response.status_mut() = StatusCode::CREATED;
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_parse_against_the_file_length() {
        assert_eq!(parse_range("bytes=0-3", 10), Some(Ok((0, 3))));
        assert_eq!(parse_range("bytes=4-", 10), Some(Ok((4, 9))));
        assert_eq!(parse_range("bytes=-3", 10), Some(Ok((7, 9))));
        assert_eq!(parse_range("bytes=2-99", 10), Some(Ok((2, 9))));
        assert_eq!(parse_range("bytes=10-", 10), Some(Err(())));
        assert_eq!(parse_range("bytes=5-2", 10), Some(Err(())));
        assert_eq!(parse_range("bytes=0-0", 0), Some(Err(())));
        assert_eq!(parse_range("bytes=0-1,4-5", 10), None);
        assert_eq!(parse_range("items=0-1", 10), None);
        assert_eq!(parse_range("bytes=a-b", 10), None);
    }

    #[test]
    fn media_types_fall_back_to_sniffing() {
        assert_eq!(media_type(Path::new("a.png"), b""), "image/png");
        assert_eq!(
            media_type(Path::new("a.html"), b""),
            "text/html; charset=utf-8"
        );
        assert_eq!(
            media_type(Path::new("notes"), b"plain words"),
            "text/plain; charset=utf-8"
        );
        assert_eq!(
            media_type(Path::new("blob.bin"), &[1, 0, 2]),
            "application/octet-stream"
        );
    }

    #[test]
    fn download_names_are_header_safe() {
        assert_eq!(
            disposition(Path::new("/p/report.pdf"), false),
            "inline; filename=\"report.pdf\""
        );
        assert_eq!(
            disposition(Path::new("/p/a\"b\nc.txt"), true),
            "attachment; filename=\"a_b_c.txt\""
        );
    }
}

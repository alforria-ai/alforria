//! URL skill sources — port of `packages/core/src/skill/discovery.ts`
//! (index fetch, safe-segment/safe-path validation, versioned caching).

use std::path::{Path, PathBuf};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// Validation helpers (`discovery.ts:15-53`)
// ---------------------------------------------------------------------------

fn is_safe_segment(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains('/')
        && !value.contains('\\')
        && !value.contains('\0')
}

/// `isSafeRelativePath` (`discovery.ts:26-53`).
fn is_safe_relative_path(value: &str) -> bool {
    if value.is_empty()
        || value.contains('\\')
        || value.contains('\0')
        || value.contains('?')
        || value.contains('#')
        || url_can_parse(value)
        || value.starts_with('/')
    {
        return false;
    }
    value.split('/').all(|segment| {
        let Ok(decoded) = percent_decode(segment) else {
            return false;
        };
        !decoded.is_empty()
            && decoded != "."
            && decoded != ".."
            && !decoded.contains('/')
            && !decoded.contains('\\')
            && !decoded.contains('\0')
    })
}

/// `URL.canParse(value)` without a base — true when `value` has a scheme
/// (`new URL("a:b")` parses with an opaque path).
fn url_can_parse(value: &str) -> bool {
    let Some((scheme, rest)) = value.split_once(':') else {
        return false;
    };
    !scheme.is_empty()
        && rest.len() >= 2
        && scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
}

/// `decodeURIComponent` — `Err` on invalid escapes (the TS call throws and
/// the segment is rejected).
fn percent_decode(input: &str) -> Result<String, ()> {
    let bytes = input.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            if i + 2 < bytes.len() {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).map_err(|_| ())?;
                out.push(u8::from_str_radix(hex, 16).map_err(|_| ())?);
                i += 3;
                continue;
            }
            return Err(());
        }
        out.push(bytes[i]);
        i += 1;
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

// ---------------------------------------------------------------------------
// URL resolution (`new URL(input, base)` for the discovery's shapes)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct Url {
    scheme: String,
    /// Host with optional port.
    host: String,
    path: String,
}

impl Url {
    fn parse(input: &str) -> Option<Url> {
        let (scheme, rest) = input.split_once("://")?;
        if scheme.is_empty() || rest.is_empty() {
            return None;
        }
        let (authority, path) = match rest.split_once('/') {
            Some((authority, path)) => (authority.to_string(), format!("/{path}")),
            None => (rest.to_string(), "/".to_string()),
        };
        let authority = authority.split('@').next_back()?.to_string();
        if authority.is_empty() {
            return None;
        }
        Some(Url {
            scheme: scheme.to_string(),
            host: authority,
            path,
        })
    }

    fn origin(&self) -> String {
        format!("{}://{}", self.scheme, self.host)
    }

    fn href(&self) -> String {
        format!("{}://{}{}", self.scheme, self.host, self.path)
    }

    fn join(&self, input: &str) -> Option<Url> {
        let path = if input.starts_with('/') {
            input.to_string()
        } else {
            let base = if self.path.ends_with('/') {
                self.path.clone()
            } else {
                match self.path.rfind('/') {
                    Some(index) => self.path[..index + 1].to_string(),
                    None => "/".to_string(),
                }
            };
            format!("{base}{input}")
        };
        Some(Url {
            scheme: self.scheme.clone(),
            host: self.host.clone(),
            path: simplify(&path),
        })
    }
}

fn simplify(path: &str) -> String {
    let trailing = path.len() > 1 && path.ends_with('/');
    let mut out: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            _ => out.push(segment),
        }
    }
    // `new URL` keeps a trailing slash (directory URLs stay directory URLs).
    if trailing {
        format!("/{}/", out.join("/"))
    } else {
        format!("/{}", out.join("/"))
    }
}

fn contains(root: &Path, path: &Path) -> bool {
    path.starts_with(root) && path != root
}

fn encode_uri_component(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')' => {
                out.push(*byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Bun.hash — wyhash (Zig's std.hash.Wyhash), needed for the cache directory
// name (`discovery.ts:113`, Zig `std/hash/wyhash.zig`).
// ---------------------------------------------------------------------------

fn wymum(a: u64, b: u64) -> (u64, u64) {
    let product = u128::from(a) * u128::from(b);
    (product as u64, (product >> 64) as u64)
}

fn wy_mix(a: u64, b: u64) -> u64 {
    let (lo, hi) = wymum(a, b);
    lo ^ hi
}

fn read8(input: &[u8], offset: usize) -> u64 {
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&input[offset..offset + 8]);
    u64::from_le_bytes(buf)
}

fn read4(input: &[u8], offset: usize) -> u64 {
    let mut buf = [0u8; 4];
    buf.copy_from_slice(&input[offset..offset + 4]);
    u32::from_le_bytes(buf) as u64
}

/// `Wyhash.hash(seed, input)` (Zig `std.hash.Wyhash` — the algorithm behind
/// `Bun.hash`).
pub fn bun_hash_with_seed(seed: u64, input: &[u8]) -> u64 {
    const SECRET: [u64; 4] = [
        0xa0761d6478bd642f,
        0xe7037ed1a0b428db,
        0x8ebc6af09c88c6e3,
        0x589965cc75374cc3,
    ];
    let len = input.len();
    let init = seed ^ wy_mix(seed ^ SECRET[0], SECRET[1]);
    let mut state = [init, init, init];
    let a: u64;
    let b: u64;
    if len <= 16 {
        if len >= 4 {
            let end = len - 4;
            let quarter = (len >> 3) << 2;
            a = (read4(input, 0) << 32) | read4(input, quarter);
            b = (read4(input, end) << 32) | read4(input, end - quarter);
        } else if len > 0 {
            a = u64::from(input[0]) << 16
                | u64::from(input[len >> 1]) << 8
                | u64::from(input[len - 1]);
            b = 0;
        } else {
            a = 0;
            b = 0;
        }
    } else {
        let mut i = 0usize;
        if len >= 48 {
            while i + 48 < len {
                for slot in 0..3 {
                    let offset = i + slot * 16;
                    let x = read8(input, offset) ^ SECRET[slot + 1];
                    let y = read8(input, offset + 8) ^ state[slot];
                    state[slot] = wy_mix(x, y);
                }
                i += 48;
            }
            state[0] ^= state[1] ^ state[2];
        }
        // `final1` (`wyhash.zig:155-171`): 16-byte chunks while strictly
        // more than 16 bytes remain, then the trailing 16-byte window.
        let mut j = 0usize;
        while j + 16 < len - i {
            state[0] = wy_mix(
                read8(input, i + j) ^ SECRET[1],
                read8(input, i + j + 8) ^ state[0],
            );
            j += 16;
        }
        a = read8(input, len - 16);
        b = read8(input, len - 8);
    }
    // `final2` (`wyhash.zig:173-177`).
    let mut x = a ^ SECRET[1];
    let mut y = b ^ state[0];
    let (lo, hi) = wymum(x, y);
    x = lo;
    y = hi;
    let (m, n) = wymum(x ^ SECRET[0] ^ len as u64, y ^ SECRET[1]);
    m ^ n
}

/// `Bun.hash(input)` — wyhash with the default seed 0.
pub fn bun_hash(input: &[u8]) -> u64 {
    bun_hash_with_seed(0, input)
}

// ---------------------------------------------------------------------------
// Fetch seam (`discovery.ts:76-83`)
// ---------------------------------------------------------------------------

/// The HTTP seam — `HttpClient` in TS. Tests inject doubles; production
/// uses [`ReqwestFetcher`].
pub trait UrlFetcher: Send + Sync {
    fn get(&self, url: &str) -> Result<Vec<u8>, String>;
}

/// Production fetcher: reqwest on a dedicated runtime thread, safe to call
/// from sync contexts (instance boot).
#[derive(Default)]
pub struct ReqwestFetcher;

impl UrlFetcher for ReqwestFetcher {
    fn get(&self, url: &str) -> Result<Vec<u8>, String> {
        std::thread::scope(|scope| {
            scope
                .spawn(move || {
                    tokio::runtime::Builder::new_current_thread()
                        .build()
                        .map_err(|err| err.to_string())?
                        .block_on(async {
                            let bytes = reqwest::get(url)
                                .await
                                .map_err(|err| err.to_string())?
                                .bytes()
                                .await
                                .map_err(|err| err.to_string())?;
                            Ok::<Vec<u8>, String>(bytes.to_vec())
                        })
                })
                .join()
                .map_err(|_| "fetch panicked".to_string())?
        })
    }
}

// ---------------------------------------------------------------------------
// SkillDiscovery
// ---------------------------------------------------------------------------

/// `IndexSkill` / `Index` (`discovery.ts:55-63`).
#[derive(Debug, Clone, serde::Deserialize)]
struct IndexSkill {
    name: String,
    version: Option<String>,
    files: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
struct Index {
    skills: Vec<IndexSkill>,
}

/// `SkillDiscovery.Service` (`discovery.ts:65-209`).
pub struct SkillDiscovery {
    cache_dir: PathBuf,
    fetcher: Arc<dyn UrlFetcher>,
}

/// One download target — the relative file plus its resolved URL.
struct RemoteFile {
    relative: String,
    resource: String,
}

impl SkillDiscovery {
    pub fn new(cache_dir: PathBuf, fetcher: Arc<dyn UrlFetcher>) -> SkillDiscovery {
        SkillDiscovery { cache_dir, fetcher }
    }

    /// `download` (`discovery.ts:85-96`).
    fn download(&self, url: &str, destination: &Path) -> bool {
        if destination.exists() {
            return true;
        }
        match self.fetcher.get(url) {
            Ok(body) => {
                if let Some(parent) = destination.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                std::fs::write(destination, body).is_ok()
            }
            Err(error) => {
                tracing::error!(url, error, "failed to download skill file");
                false
            }
        }
    }

    fn download_all(&self, base: &Path, files: &[RemoteFile]) -> bool {
        for file in files {
            if !self.download(&file.resource, &base.join(&file.relative)) {
                return false;
            }
        }
        true
    }

    fn skill_present(&self, root: &Path, name: &str) -> bool {
        root.join("SKILL.md").exists() || root.join(format!("{name}.md")).exists()
    }

    /// `pull` (`discovery.ts:99-207`). The TS fetch layers (retries, the
    /// 4/8-way download concurrency) collapse into sequential fetches over
    /// the same seam — the resulting directories are identical.
    pub fn pull(&self, url: &str) -> Vec<PathBuf> {
        let base = if url.ends_with('/') {
            url.to_string()
        } else {
            format!("{url}/")
        };
        let Some(source) = Url::parse(&base) else {
            return Vec::new();
        };
        let Some(index_url) = source.join("index.json") else {
            return Vec::new();
        };
        let index_url = index_url.href();
        let data = match self.fetcher.get(&index_url).ok().map(|body| {
            serde_json::from_slice::<Index>(&body)
                .map_err(|_| ())
                .ok()
                .ok_or(())
        }) {
            Some(Ok(index)) => index,
            _ => {
                tracing::error!(url = index_url, "failed to fetch skill index");
                return Vec::new();
            }
        };

        let source_root = self
            .cache_dir
            .join("skills")
            .join(format!("{:x}", bun_hash(base.as_bytes())));
        let mut out = Vec::new();
        for skill in data.skills {
            if !is_safe_segment(&skill.name) {
                continue;
            }
            if !skill.files.iter().any(|file| file == "SKILL.md")
                && !skill
                    .files
                    .iter()
                    .any(|file| file == &format!("{}.md", skill.name))
            {
                continue;
            }
            let root = source_root.join(&skill.name);
            if !contains(&source_root, &root) {
                continue;
            }
            let Some(skill_url) = source.join(&format!("{}/", encode_uri_component(&skill.name)))
            else {
                continue;
            };
            let mut files: Vec<RemoteFile> = Vec::new();
            let mut ok = true;
            for file in &skill.files {
                if !is_safe_relative_path(file) {
                    ok = false;
                    break;
                }
                let Some(resource) = skill_url.join(file) else {
                    ok = false;
                    break;
                };
                if resource.origin() != source.origin() || !contains(&root, &root.join(file)) {
                    ok = false;
                    break;
                }
                files.push(RemoteFile {
                    relative: file.clone(),
                    resource: resource.href(),
                });
            }
            if !ok {
                continue;
            }

            match skill.version.as_deref() {
                None => {
                    self.download_all(&root, &files);
                }
                Some(version) => {
                    let current = std::fs::read_to_string(root.join(".opencode-version")).ok();
                    if current.as_deref() != Some(version) {
                        self.refresh(&skill, &root, version, &files);
                    }
                }
            }

            if self.skill_present(&root, &skill.name) {
                out.push(root);
            }
        }
        out
    }

    /// The versioned staging swap (`discovery.ts:166-199`).
    fn refresh(&self, skill: &IndexSkill, root: &Path, version: &str, files: &[RemoteFile]) {
        let name = root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let token = ulid::Ulid::new();
        let staging = root.with_file_name(format!("{name}.tmp-{token}"));
        let backup = root.with_file_name(format!("{name}.old-{token}"));
        let result = (|| -> bool {
            if !self.download_all(&staging, files) {
                return false;
            }
            if !self.skill_present(&staging, &skill.name) {
                return false;
            }
            if std::fs::write(staging.join(".opencode-version"), version).is_err() {
                return false;
            }
            let cached = root.exists();
            if cached && std::fs::rename(root, &backup).is_err() {
                return false;
            }
            if std::fs::rename(&staging, root).is_err() {
                if cached {
                    let _ = std::fs::rename(&backup, root);
                }
                return false;
            }
            if cached {
                let _ = std::fs::remove_dir_all(&backup);
            }
            true
        })();
        if !result {
            tracing::error!(skill = skill.name, "failed to refresh skill");
        }
        let _ = std::fs::remove_dir_all(&staging);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wyhash_test_vectors() {
        // Zig std.hash.Wyhash test vectors (the algorithm Bun.hash uses).
        let vectors: [(u64, &str, u64); 7] = [
            (0, "", 0x409638ee2bde459),
            (1, "a", 0xa8412d091b5fe0a9),
            (2, "abc", 0x32dd92e4b2915153),
            (3, "message digest", 0x8619124089a3a16b),
            (4, "abcdefghijklmnopqrstuvwxyz", 0x7a43afb61d7f5f40),
            (
                5,
                "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
                0xff42329b90e50d58,
            ),
            (
                6,
                "12345678901234567890123456789012345678901234567890123456789012345678901234567890",
                0xc39cab13b115aad3,
            ),
        ];
        for (seed, input, expected) in vectors {
            assert_eq!(
                bun_hash_with_seed(seed, input.as_bytes()),
                expected,
                "{input}"
            );
        }
    }

    #[test]
    fn safe_relative_path_matrix() {
        assert!(is_safe_relative_path("SKILL.md"));
        assert!(is_safe_relative_path("docs/intro.md"));
        assert!(is_safe_relative_path("a%20b/c.md"));
        assert!(!is_safe_relative_path(""));
        assert!(!is_safe_relative_path("../escape.md"));
        assert!(!is_safe_relative_path("a/../escape.md"));
        assert!(!is_safe_relative_path("/absolute.md"));
        assert!(!is_safe_relative_path("back\\slash.md"));
        assert!(!is_safe_relative_path("query?mark.md"));
        assert!(!is_safe_relative_path("hash#mark.md"));
        assert!(!is_safe_relative_path("https://evil"));
        assert!(!is_safe_relative_path("mailto:someone"));
        assert!(!is_safe_relative_path("a%2Fb.md"));
    }

    #[test]
    fn url_join_matrix() {
        let base = Url::parse("https://example.com/skills/").unwrap();
        assert_eq!(
            base.join("index.json").unwrap().href(),
            "https://example.com/skills/index.json"
        );
        let skill = base.join("my%20skill/").unwrap();
        assert_eq!(
            skill.join("SKILL.md").unwrap().href(),
            "https://example.com/skills/my%20skill/SKILL.md"
        );
        assert_eq!(skill.origin(), "https://example.com");
        assert!(Url::parse("https://example.com").is_some());
        assert!(Url::parse("not a url").is_none());
    }

    /// A fetcher double serving a static map of url → bytes.
    struct StubFetcher {
        routes: std::sync::Mutex<std::collections::HashMap<String, Result<Vec<u8>, String>>>,
    }

    impl UrlFetcher for StubFetcher {
        fn get(&self, url: &str) -> Result<Vec<u8>, String> {
            self.routes
                .lock()
                .unwrap()
                .get(url)
                .cloned()
                .unwrap_or_else(|| Err("not found".to_string()))
        }
    }

    fn discovery_with(routes: &[(&str, &str)]) -> (SkillDiscovery, std::sync::Arc<StubFetcher>) {
        let mut routes_map = std::collections::HashMap::new();
        for (url, body) in routes {
            routes_map.insert((*url).to_string(), Ok(body.as_bytes().to_vec()));
        }
        let fetcher = std::sync::Arc::new(StubFetcher {
            routes: std::sync::Mutex::new(routes_map),
        });
        (
            SkillDiscovery::new(
                std::env::temp_dir().join(format!("skills-test-{}", ulid::Ulid::new())),
                fetcher.clone(),
            ),
            fetcher,
        )
    }

    const INDEX: &str = r#"{"skills":[{"name":"my-skill","version":"1","files":["SKILL.md"]}]}"#;

    #[test]
    fn pull_downloads_skills_from_the_index() {
        let (discovery, _fetcher) = discovery_with(&[
            ("https://example.com/skills/index.json", INDEX),
            (
                "https://example.com/skills/my-skill/SKILL.md",
                "---\nname: my-skill\n---\nhello\n",
            ),
        ]);
        let dirs = discovery.pull("https://example.com/skills");
        assert_eq!(dirs.len(), 1);
        let root = &dirs[0];
        assert!(root.ends_with("my-skill"));
        assert!(root.join("SKILL.md").exists());
        assert!(std::fs::read_to_string(root.join(".opencode-version"))
            .unwrap()
            .starts_with("1"));

        // Second pull: cached files stay on disk.
        let dirs = discovery.pull("https://example.com/skills/");
        assert_eq!(dirs.len(), 1);
    }

    #[test]
    fn pull_rejects_unsafe_skill_names() {
        let index = r#"{"skills":[{"name":"../escape","files":["SKILL.md"]}]}"#;
        let (discovery, _fetcher) =
            discovery_with(&[("https://example.com/skills/index.json", index)]);
        let dirs = discovery.pull("https://example.com/skills");
        assert!(dirs.is_empty());
    }

    #[test]
    fn pull_requires_skill_markdown() {
        let index = r#"{"skills":[{"name":"my-skill","files":["README.md"]}]}"#;
        let (discovery, _fetcher) =
            discovery_with(&[("https://example.com/skills/index.json", index)]);
        let dirs = discovery.pull("https://example.com/skills");
        assert!(dirs.is_empty());
    }

    #[test]
    fn pull_swallows_missing_index() {
        let (discovery, _fetcher) = discovery_with(&[]);
        let dirs = discovery.pull("https://example.com/skills");
        assert!(dirs.is_empty());
    }

    #[test]
    fn pull_rejects_escaping_file_paths() {
        let index = r#"{"skills":[{"name":"my-skill","files":["../SKILL.md"]}]}"#;
        let (discovery, _fetcher) =
            discovery_with(&[("https://example.com/skills/index.json", index)]);
        let dirs = discovery.pull("https://example.com/skills");
        assert!(dirs.is_empty());
    }

    #[test]
    fn pull_refreshes_on_version_change() {
        let (discovery, fetcher) = discovery_with(&[
            ("https://example.com/skills/index.json", INDEX),
            (
                "https://example.com/skills/my-skill/SKILL.md",
                "---\nname: my-skill\n---\nv1\n",
            ),
        ]);
        let dirs = discovery.pull("https://example.com/skills");
        assert_eq!(dirs.len(), 1);
        let root = dirs[0].clone();
        assert_eq!(
            std::fs::read_to_string(root.join("SKILL.md")).unwrap(),
            "---\nname: my-skill\n---\nv1\n"
        );

        // Same version → no re-download of changed bytes.
        let changed = INDEX.replace("\"1\"", "\"1.1\"");
        let v2 = "---\nname: my-skill\n---\nv2\n";
        fetcher.routes.lock().unwrap().insert(
            "https://example.com/skills/index.json".to_string(),
            Ok(changed.as_bytes().to_vec()),
        );
        fetcher.routes.lock().unwrap().insert(
            "https://example.com/skills/my-skill/SKILL.md".to_string(),
            Ok(v2.as_bytes().to_vec()),
        );
        let dirs = discovery.pull("https://example.com/skills");
        assert_eq!(dirs.len(), 1);
        assert_eq!(
            std::fs::read_to_string(dirs[0].join("SKILL.md")).unwrap(),
            v2,
            "the version bump swaps the cached directory"
        );
    }
}

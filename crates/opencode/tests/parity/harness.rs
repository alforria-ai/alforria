//! TS source resolution + dual-binary spawn plumbing (spec PARITY §2.2).

use std::io::BufRead;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::harness::Env;

/// The pinned TS reference commit (see `fixtures/PINNED.md`).
pub const PINNED_COMMIT: &str = "88c6c7abc7f320b6aabed2634ac0b2d6e6ecea67";

/// `OPENCODE_PARITY=1` gates every parity test; without it the tests
/// early-return in <1s.
pub fn gated() -> bool {
    std::env::var("OPENCODE_PARITY").ok().as_deref() == Some("1")
}

/// A verified TS reference checkout at the pinned commit.
pub struct TsSource {
    pub root: PathBuf,
}

impl TsSource {
    /// Resolve and pin-verify the TS clone: `$OPENCODE_TS_SRC` or
    /// `/tmp/opencode-src`. Mismatched pin ⇒ fail fast, never compare
    /// against a moving target.
    pub fn resolve() -> TsSource {
        let root = PathBuf::from(
            std::env::var("OPENCODE_TS_SRC").unwrap_or_else(|_| "/tmp/opencode-src".to_string()),
        );
        let bun = Command::new("bun")
            .arg("--version")
            .output()
            .expect("bun must be on PATH for parity runs");
        assert!(
            bun.status.success(),
            "bun --version failed: {}",
            String::from_utf8_lossy(&bun.stderr)
        );
        assert!(
            root.is_dir(),
            "TS clone missing: {} — re-clone the pinned commit or set OPENCODE_TS_SRC",
            root.display()
        );
        assert!(
            root.join("packages/opencode/src/index.ts").is_file(),
            "TS clone at {} has no packages/opencode/src/index.ts",
            root.display()
        );
        let head = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&root)
            .output()
            .expect("git rev-parse HEAD");
        assert!(
            head.status.success(),
            "git rev-parse HEAD failed in {}",
            root.display()
        );
        let commit = String::from_utf8_lossy(&head.stdout).trim().to_string();
        assert_eq!(
            commit,
            PINNED_COMMIT,
            "TS clone at {} is at {commit}, not the pinned {PINNED_COMMIT}; \
             re-clone or set OPENCODE_TS_SRC to the pinned checkout",
            root.display()
        );
        TsSource { root }
    }
}

/// Pre-pick a free loopback port (the TS CLI has no port-0 discovery).
pub fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind for free port")
        .local_addr()
        .expect("local addr")
        .port()
}

/// A running `bun … serve` bound to a loopback port, killed on drop.
pub struct TsServe {
    pub port: u16,
    stderr: Arc<Mutex<String>>,
    child: Child,
}

impl TsServe {
    /// Spawn the TS server inside the given project env and wait for the
    /// `listening on` handshake line. `--cwd` resolves the TS package
    /// (module resolution), `current_dir` gives the server its project
    /// workspace.
    #[allow(clippy::zombie_processes)]
    pub fn spawn(ts: &TsSource, env: &Env) -> TsServe {
        let port = free_port();
        let cwd = ts.root.join("packages/opencode");
        let mut command = Command::new("bun");
        command
            .args([
                "run",
                "--cwd",
                cwd.to_str().expect("utf-8 path"),
                "src/index.ts",
                "serve",
                "--hostname",
                "127.0.0.1",
                "--port",
                &port.to_string(),
            ])
            .current_dir(env.project_dir())
            .env("OPENCODE_TEST_HOME", &env.home)
            .env("XDG_CONFIG_HOME", env.home.join(".config"))
            .env("XDG_DATA_HOME", env.home.join(".local/share"))
            .env("XDG_CACHE_HOME", env.home.join(".cache"))
            .env("XDG_STATE_HOME", env.home.join(".local/state"))
            .env("OPENCODE_MODELS_PATH", &env.models_path)
            .env("OPENCODE_DISABLE_MODELS_FETCH", "1")
            .env_remove("OPENCODE_SERVER_PASSWORD")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("spawn bun serve");
        let stdout = child.stdout.take().expect("serve stdout");
        let stderr = child.stderr.take().expect("serve stderr");
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let _ = tx.send(line);
            }
        });
        let errors = Arc::new(Mutex::new(String::new()));
        let sink = errors.clone();
        std::thread::spawn(move || {
            use std::io::Read;
            let mut text = String::new();
            let mut pipe = stderr;
            let _ = pipe.read_to_string(&mut text);
            *sink.lock().unwrap() = text;
        });
        let handshake = format!("opencode server listening on http://127.0.0.1:{port}");
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let line = rx.recv_timeout(Duration::from_secs(1));
            match line {
                Ok(line) if line == handshake => {
                    let stderr = errors.clone();
                    return TsServe {
                        port,
                        stderr,
                        child,
                    };
                }
                Ok(_) => {}
                Err(_) => {
                    assert!(
                        Instant::now() < deadline,
                        "bun serve handshake timed out; stderr={}",
                        errors.lock().unwrap()
                    );
                }
            }
        }
    }
}

impl Drop for TsServe {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if std::env::var("OPENCODE_PARITY_DEBUG").is_ok() {
            std::thread::sleep(Duration::from_millis(200));
            eprintln!("[ts serve stderr] {}", self.stderr.lock().unwrap());
        }
    }
}

/// Run one TS CLI command (`export`, `import`, …) in the given project
/// env — the side-abstracted twin of `Env::run` for the parity drivers.
/// Unlike [`TsServe::spawn`] (where the request `directory` param pins
/// the project), the CLI commands resolve their project from the process
/// cwd — and bun's `--cwd` flag *changes* the process cwd, so the entry
/// point is passed by absolute path from the project directory instead.
pub fn ts_cli(ts: &TsSource, env: &Env, args: &[&str]) -> crate::harness::ProcOutput {
    let script = ts.root.join("packages/opencode/src/index.ts");
    let mut command = Command::new("bun");
    command.arg("run").arg(script);
    for arg in args {
        command.arg(arg);
    }
    command
        .current_dir(env.project_dir())
        .env("OPENCODE_TEST_HOME", &env.home)
        .env("XDG_CONFIG_HOME", env.home.join(".config"))
        .env("XDG_DATA_HOME", env.home.join(".local/share"))
        .env("XDG_CACHE_HOME", env.home.join(".cache"))
        .env("XDG_STATE_HOME", env.home.join(".local/state"))
        .env("OPENCODE_MODELS_PATH", &env.models_path)
        .env("OPENCODE_DISABLE_MODELS_FETCH", "1")
        .env_remove("OPENCODE_SERVER_PASSWORD")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("spawn bun cli");
    crate::harness::wait(&mut child, Duration::from_secs(180))
}

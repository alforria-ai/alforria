//! Shared e2e harness (spec E2E §2.1): the `Env` spawn/pump helpers
//! lifted from `tests/e2e.rs` (single source of truth — `e2e.rs` includes
//! this file via `#[path]`).

use std::io::{BufRead, Read};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use axum::Router;

/// The models-dev `api.json` fixture shared by every test env.
pub const API_JSON: &str = r#"{
  "anthropic": {
    "name": "Anthropic",
    "env": ["ANTHROPIC_API_KEY"],
    "id": "anthropic",
    "npm": "@anthropic-ai/sdk",
    "models": {
      "claude-sonnet-4-5": {
        "id": "claude-sonnet-4-5",
        "name": "Claude Sonnet 4.5",
        "release_date": "2025-09-29",
        "attachment": true,
        "reasoning": true,
        "temperature": true,
        "tool_call": true,
        "limit": {"context": 100000, "output": 4096}
      }
    }
  }
}"#;

/// A running HTTP mock server bound to a loopback port.
pub struct MockServer {
    pub port: u16,
}

impl MockServer {
    pub fn new(router: Router) -> MockServer {
        let (port_tx, port_rx) = std::sync::mpsc::channel::<u16>();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime");
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("bind");
                let port = listener.local_addr().expect("addr").port();
                port_tx.send(port).expect("send port");
                axum::serve(listener, router).await.expect("serve");
            });
        });
        let port = port_rx.recv().expect("mock server port");
        MockServer { port }
    }
}

/// One e2e environment, isolated HOME + XDG dirs and a project directory
/// whose `opencode.json` wires the mock provider at the given LLM URL.
pub struct Env {
    pub _dir: tempfile::TempDir,
    pub home: PathBuf,
    pub models_path: PathBuf,
}

impl Env {
    pub fn new(tag: &str) -> Env {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).expect("create home");
        let models_path = dir.path().join("api.json");
        std::fs::write(&models_path, API_JSON).expect("write fixture");
        let env = Env {
            _dir: dir,
            home,
            models_path,
        };
        env.write_project_config("http://unused.invalid", tag);
        env
    }

    pub fn project_dir(&self) -> PathBuf {
        self.home.join("project")
    }

    pub fn write_project_config(&self, llm_url: &str, tag: &str) {
        self.write_provider_config(llm_url, tag, "test-key", "mock", "mock-model", 100_000.0);
    }

    /// The backend-parametrized variant: wires the given provider at the
    /// given base URL (spec E2E §2.2).
    #[allow(clippy::too_many_arguments)]
    pub fn write_provider_config(
        &self,
        llm_url: &str,
        tag: &str,
        api_key: &str,
        provider_id: &str,
        model_id: &str,
        context_limit: f64,
    ) {
        let project = self.project_dir();
        std::fs::create_dir_all(&project).expect("create project");
        let context = if context_limit.fract() == 0.0 {
            serde_json::json!(context_limit as i64)
        } else {
            serde_json::json!(context_limit)
        };
        let config = serde_json::json!({
            "provider": {
                provider_id: {
                    "options": {
                        "apiKey": api_key,
                        "baseURL": llm_url,
                    },
                    "models": {
                        model_id: {
                            "name": "Mock Model",
                            "limit": {"context": context, "output": 4096},
                        },
                    },
                },
            },
            "share": "disabled",
            "title": format!("e2e {tag}"),
        });
        std::fs::write(
            project.join("opencode.json"),
            serde_json::to_string_pretty(&config).expect("serialize"),
        )
        .expect("write opencode.json");
    }

    pub fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_opencode"));
        command
            .args(args)
            .env("OPENCODE_TEST_HOME", &self.home)
            .env("PWD", self.project_dir())
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("XDG_DATA_HOME", self.home.join(".local/share"))
            .env("XDG_CACHE_HOME", self.home.join(".cache"))
            .env("XDG_STATE_HOME", self.home.join(".local/state"))
            .env("OPENCODE_MODELS_PATH", &self.models_path)
            .env("OPENCODE_DISABLE_MODELS_FETCH", "1")
            .env_remove("OPENCODE_SERVER_PASSWORD")
            .current_dir(self.project_dir());
        command
    }

    pub fn run(&self, args: &[&str]) -> ProcOutput {
        self.run_with(args, None)
    }

    pub fn run_with(&self, args: &[&str], stdin: Option<&str>) -> ProcOutput {
        self.run_with_timeout(args, stdin, Duration::from_secs(180))
    }

    /// The backend-budgeted variant: live models need per-turn headroom
    /// (spec E2E §3.4) beyond the mock's 180 s.
    pub fn run_with_timeout(
        &self,
        args: &[&str],
        stdin: Option<&str>,
        timeout: Duration,
    ) -> ProcOutput {
        let mut command = self.command(args);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        if stdin.is_some() {
            command.stdin(Stdio::piped());
        }
        let mut child = command.spawn().expect("spawn opencode");
        if let Some(stdin) = stdin {
            use std::io::Write;
            let mut pipe = child.stdin.take().expect("stdin");
            pipe.write_all(stdin.as_bytes()).expect("write stdin");
            drop(pipe);
        }
        let ProcOutput {
            code,
            stdout,
            stderr,
        } = wait(&mut child, timeout);
        ProcOutput {
            code,
            stdout,
            stderr,
        }
    }
}

#[derive(Debug)]
pub struct ProcOutput {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// A running `opencode serve` bound to a loopback port, killed on drop.
pub struct Serve {
    pub port: u16,
    child: Child,
}

impl Serve {
    /// Spawn `serve --port 0` and wait for the handshake line.
    // The child is killed and waited on in `Drop`; the lint cannot see
    // through the struct handle.
    #[allow(clippy::zombie_processes)]
    pub fn spawn(env: &Env) -> Serve {
        let mut child = env
            .command(&["serve", "--port", "0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn serve");
        let stdout = child.stdout.take().expect("serve stdout");
        let mut stderr = child.stderr.take().expect("serve stderr");
        // Drain both pipes from reader threads so a full pipe can never
        // block the child.
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let _ = tx.send(line);
            }
        });
        std::thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr.read_to_string(&mut text);
        });
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let line = rx
                .recv_timeout(Duration::from_secs(1))
                .expect("serve handshake line");
            if let Some(rest) = line.strip_prefix("opencode server listening on ") {
                let port = rest
                    .trim_start_matches("http://")
                    .rsplit(':')
                    .next()
                    .map(|port| port.parse::<u16>().expect("port"))
                    .expect("handshake port");
                return Serve { port, child };
            }
            assert!(Instant::now() < deadline, "serve handshake timed out");
        }
    }
}

impl Drop for Serve {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Wait for the child, draining stdout/stderr in reader threads (a
/// blocking child would otherwise deadlock on a full pipe).
pub fn wait(child: &mut Child, timeout: Duration) -> ProcOutput {
    let stdout = child.stdout.take().map(pump);
    let stderr = child.stderr.take().map(pump);
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait().expect("wait status") {
            Some(status) => {
                return ProcOutput {
                    code: status.code(),
                    stdout: stdout.map(read_pump).unwrap_or_default(),
                    stderr: stderr.map(read_pump).unwrap_or_default(),
                };
            }
            None => {
                assert!(Instant::now() < deadline, "opencode timed out");
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn pump<R: Read + Send + 'static>(mut pipe: R) -> std::sync::Arc<std::sync::Mutex<String>> {
    let buffer = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let shared = buffer.clone();
    std::thread::spawn(move || {
        let mut text = String::new();
        pipe.read_to_string(&mut text).ok();
        *shared.lock().unwrap() = text;
    });
    buffer
}

fn read_pump(buffer: std::sync::Arc<std::sync::Mutex<String>>) -> String {
    std::mem::take(&mut *buffer.lock().unwrap())
}

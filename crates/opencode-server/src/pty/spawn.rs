//! PTY spawn backend — the `portable-pty` seam. Port of the
//! `#pty` interface (`core/src/pty/pty.ts`) and its node-pty
//! implementation (`core/src/pty/pty.node.ts`); the concrete backend
//! (`spawn.rs`) isolates the `portable-pty` dependency (spec M6 §9 S3).

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};

/// `Opts` (`pty.ts:10-16`) — the overrides the caller needs; the backend
/// layers them over its inherited process environment.
pub struct SpawnOpts {
    pub cwd: PathBuf,
    pub env: BTreeMap<String, String>,
}

/// `Exit` (`pty.ts:5-8`).
#[derive(Debug, Clone, Copy)]
pub struct Exit {
    pub exit_code: Option<u64>,
}

/// `Proc` (`pty.ts:18-25`). Listeners fire from dedicated reader/waiter
/// threads and must stay non-blocking.
pub trait Proc: Send {
    fn pid(&self) -> u32;
    fn write(&self, data: &str);
    fn resize(&self, cols: u16, rows: u16);
    fn kill(&self);
    fn on_data(&self, listener: Box<dyn Fn(&str) + Send>);
    fn on_exit(&self, listener: Box<dyn Fn(Exit) + Send>);
}

/// `spawn` (`pty.node.ts:6-29`) behind a trait object.
pub trait SpawnBackend: Send + Sync {
    fn spawn(&self, file: &str, args: &[String], opts: &SpawnOpts)
        -> Result<Box<dyn Proc>, String>;
}

/// The production backend over `portable-pty`.
pub fn portable_backend() -> Arc<dyn SpawnBackend> {
    Arc::new(PortableBackend)
}

struct PortableBackend;

impl SpawnBackend for PortableBackend {
    fn spawn(
        &self,
        file: &str,
        args: &[String],
        opts: &SpawnOpts,
    ) -> Result<Box<dyn Proc>, String> {
        let pair = native_pty_system()
            .openpty(PtySize::default())
            .map_err(|err| err.to_string())?;
        let mut cmd = CommandBuilder::new(file);
        for arg in args {
            cmd.arg(arg);
        }
        cmd.cwd(&opts.cwd);
        for (key, value) in &opts.env {
            cmd.env(key, value);
        }
        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|err| err.to_string())?;
        let pid = child.process_id().unwrap_or(0);
        let writer = pair.master.take_writer().map_err(|err| err.to_string())?;
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|err| err.to_string())?;
        let killer = child.clone_killer();

        let listeners = Arc::new(Listeners::default());
        spawn_reader(reader, Arc::clone(&listeners));
        spawn_waiter(child, Arc::clone(&listeners));
        Ok(Box::new(PortableProc {
            pid,
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            killer: Mutex::new(killer),
            listeners,
        }))
    }
}

type DataListeners = Vec<Box<dyn Fn(&str) + Send>>;
type ExitListeners = Vec<Box<dyn Fn(Exit) + Send>>;

#[derive(Default)]
struct Listeners {
    data: Mutex<DataListeners>,
    exit: Mutex<ExitListeners>,
}

impl Listeners {
    fn emit_data(&self, chunk: &str) {
        for listener in self.data.lock().unwrap_or_else(|p| p.into_inner()).iter() {
            listener(chunk);
        }
    }

    fn emit_exit(&self, exit: Exit) {
        for listener in self.exit.lock().unwrap_or_else(|p| p.into_inner()).iter() {
            listener(exit);
        }
    }
}

struct PortableProc {
    pid: u32,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    listeners: Arc<Listeners>,
}

impl Proc for PortableProc {
    fn pid(&self) -> u32 {
        self.pid
    }

    fn write(&self, data: &str) {
        let mut writer = self.writer.lock().unwrap_or_else(|p| p.into_inner());
        let _ = writer.write_all(data.as_bytes());
        let _ = writer.flush();
    }

    fn resize(&self, cols: u16, rows: u16) {
        let master = self.master.lock().unwrap_or_else(|p| p.into_inner());
        let _ = master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    fn kill(&self) {
        let mut killer = self.killer.lock().unwrap_or_else(|p| p.into_inner());
        let _ = killer.kill();
    }

    fn on_data(&self, listener: Box<dyn Fn(&str) + Send>) {
        self.listeners
            .data
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(listener);
    }

    fn on_exit(&self, listener: Box<dyn Fn(Exit) + Send>) {
        self.listeners
            .exit
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(listener);
    }
}

fn spawn_reader(mut reader: Box<dyn Read + Send>, listeners: Arc<Listeners>) {
    std::thread::spawn(move || {
        let mut decoder = Utf8Decoder::default();
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => {
                    let chunk = decoder.feed(&buf[..n]);
                    if !chunk.is_empty() {
                        listeners.emit_data(&chunk);
                    }
                }
                Err(_) => return,
            }
        }
    });
}

fn spawn_waiter(mut child: Box<dyn portable_pty::Child + Send + Sync>, listeners: Arc<Listeners>) {
    std::thread::spawn(move || {
        let exit_code = match child.wait() {
            Ok(status) => Some(u64::from(status.exit_code())),
            Err(_) => None,
        };
        listeners.emit_exit(Exit { exit_code });
    });
}

/// Terminal output decodes as UTF-8; reads may split multi-byte sequences
/// and invalid bytes become U+FFFD replacements.
#[derive(Default)]
struct Utf8Decoder {
    pending: Vec<u8>,
}

impl Utf8Decoder {
    fn feed(&mut self, chunk: &[u8]) -> String {
        self.pending.extend_from_slice(chunk);
        let mut out = String::new();
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(text) => {
                    out.push_str(text);
                    self.pending.clear();
                    return out;
                }
                Err(err) => {
                    let valid = err.valid_up_to();
                    if valid > 0 {
                        out.push_str(std::str::from_utf8(&self.pending[..valid]).expect("checked"));
                    }
                    match err.error_len() {
                        Some(_) => {
                            out.push('\u{FFFD}');
                            self.pending.drain(..valid + 1);
                        }
                        None => {
                            self.pending.drain(..valid);
                            return out;
                        }
                    }
                }
            }
        }
    }
}

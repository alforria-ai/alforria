//! Process launch — port of `lsp/launch.ts`.

use tokio::process::Command;

use crate::lsp::server::Handle;

/// `spawn` (launch.ts:6-21) — spawn a language server with piped stdio.
pub fn spawn(
    cmd: &str,
    args: &[String],
    cwd: &std::path::Path,
    env: Option<&std::collections::BTreeMap<String, String>>,
) -> std::io::Result<Handle> {
    let mut command = Command::new(cmd);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    if let Some(env) = env {
        for (key, value) in env {
            command.env(key, value);
        }
    }
    let child = command.spawn()?;
    Ok(Handle {
        child,
        initialization: None,
    })
}

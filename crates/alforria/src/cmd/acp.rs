//! `acp` — start the ACP (Agent Client Protocol) server
//! (`cli/cmd/acp.ts:7-73`): boot the HTTP server, then bridge ACP
//! JSON-RPC over stdio to it.

use std::ffi::OsString;
use std::sync::Arc;

use clap::ArgMatches;

use crate::error::TypedError;
use crate::network::{self, NetworkOptions};
use crate::ui::Ui;

/// The TS handler: resolve network options, listen, wire the ACP agent
/// over stdin/stdout, run until stdin ends.
pub fn run(matches: &ArgMatches, _ui: &mut Ui, raw: &[OsString]) -> Result<(), TypedError> {
    let cwd = matches
        .get_one::<String>("cwd")
        .cloned()
        .unwrap_or_else(|| {
            std::env::current_dir()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
    std::env::set_var("OPENCODE_CLIENT", "acp");

    let options = NetworkOptions::from_matches(matches);
    let resolved = network::resolve(&options, raw, &network::global_server_config());
    let runtime = super::runtime()?;
    runtime.block_on(async move {
        // The server is this process's own, but its port is open to
        // everything else on the machine: without a password any local
        // process could drive the agent. Unset means generate one, as
        // `serve` does; an empty value stays the explicit opt-out.
        let env_password = std::env::var("OPENCODE_SERVER_PASSWORD").ok();
        let generated = super::serve::generated_password_for(env_password.as_deref());
        let mut listen = resolved.listen_options();
        listen.password = generated.clone();
        let authorization = crate::client::auth_header(
            generated.as_deref().or(env_password.as_deref()),
            std::env::var("OPENCODE_SERVER_USERNAME").ok().as_deref(),
        );
        let listener =
            alforria_server::listen(&listen)
                .await
                .map_err(|err| TypedError::Unknown {
                    raw: err.to_string(),
                })?;

        let connection = Arc::new(crate::acp::jsonrpc::Connection::new(Arc::new(
            crate::acp::jsonrpc::StdioTransport::new(),
        )));
        let server = crate::acp::server::ServerClient::new(listener.port, authorization);
        let _ = cwd;
        let agent = crate::acp::AcpAgent::new(server, connection);
        agent.run().await;
        Ok::<(), TypedError>(())
    })?;
    Ok(())
}

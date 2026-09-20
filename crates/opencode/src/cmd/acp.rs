//! `acp` — start the ACP (Agent Client Protocol) server
//! (`cli/cmd/acp.ts:7-73`).

use std::ffi::OsString;

use clap::ArgMatches;

use crate::error::{CliError, TypedError};
use crate::network::{self, NetworkOptions};
use crate::ui::Ui;

/// The TS ACP agent (`@/acp/agent`, ~3.7k LOC over the
/// `@agentclientprotocol/sdk` wire protocol) has not been ported; the
/// command boots the server (matching the TS handler up to the
/// `AgentSideConnection`) and then reports the gap instead of
/// approximating the protocol.
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
    let _listener = runtime
        .block_on(opencode_server::listen(&resolved.listen_options()))
        .map_err(|err| TypedError::Unknown {
            raw: err.to_string(),
        })?;
    let _ = cwd;

    Err(TypedError::Cli(CliError {
        message: "ACP agent is not yet ported (acp/agent, ~3.7k LOC) — see docs/plans".to_string(),
        exit_code: 1,
    }))
}

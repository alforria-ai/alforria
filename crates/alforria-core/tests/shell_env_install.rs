//! Installing a policy never relaxes the process's shell posture.
//!
//! The posture is process-wide, so this file holds one test.

use alforria_core::shell_env::{self, ShellEnvPolicy};

/// A process whose posture was restricted from startup (bearer auth, say)
/// stays restricted when a later server context in the same process
/// installs a standalone policy: nothing was "tightened" after the start,
/// and the posture still must not relax.
#[test]
fn installing_a_standalone_policy_does_not_relax_a_posture_restricted_from_startup() {
    shell_env::install(ShellEnvPolicy::default().restricted(true));
    assert!(shell_env::current().is_restricted());
    assert_eq!(shell_env::tightened(), [""; 0]);

    shell_env::install(ShellEnvPolicy::default());
    assert!(
        shell_env::current().is_restricted(),
        "a later install relaxed the posture"
    );
    // The ratchet is not a reason of its own.
    assert_eq!(shell_env::tightened(), [""; 0]);
}

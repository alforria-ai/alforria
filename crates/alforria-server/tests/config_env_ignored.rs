//! A server process does not read `OPENCODE_CONFIG`, `OPENCODE_CONFIG_DIR`,
//! `OPENCODE_CONFIG_CONTENT` or `OPENCODE_PERMISSION` from its environment:
//! no entry point hands them to the config loader. So they are not a host
//! source that could change under a running server — and not a way to
//! configure one either.
//!
//! Alone in its file: it sets the variables for its whole process.

mod support;

use alforria_schema::permission_v1::PermissionV1Action;
use serde_json::json;

use support::fixture_with;

#[tokio::test]
async fn the_config_environment_variables_have_no_effect_on_a_server() {
    let scratch = tempfile::tempdir().unwrap();
    let deny = json!({ "permission": { "bash": "deny" }, "model": "env/model" }).to_string();
    let file = scratch.path().join("env-config.json");
    std::fs::write(&file, &deny).unwrap();
    let dir = scratch.path().join("env-config-dir");
    std::fs::create_dir_all(dir.join("agent")).unwrap();
    std::fs::write(dir.join("opencode.json"), &deny).unwrap();
    std::fs::write(
        dir.join("agent").join("fromenv.md"),
        "---\ndescription: from the environment\n---\nHello.\n",
    )
    .unwrap();
    // Before any server exists in this process, as a supervisor would.
    // SAFETY: the only test in this binary; nothing else reads the
    // environment concurrently yet.
    unsafe {
        std::env::set_var("OPENCODE_CONFIG", &file);
        std::env::set_var("OPENCODE_CONFIG_DIR", &dir);
        std::env::set_var("OPENCODE_CONFIG_CONTENT", &deny);
        std::env::set_var("OPENCODE_PERMISSION", r#"{"bash":"deny"}"#);
    }

    // Restricted (frozen host config) and a developer's own server alike.
    for host in [
        json!({ "server": { "trust": { "project": "restricted" } } }),
        json!({}),
    ] {
        let f = fixture_with(|_| host.clone(), None, Vec::new());
        let services = f.ctx.instances.load(&f.dirs.repo).unwrap();
        let build = services.agents.get("build").unwrap();
        assert_eq!(
            build.permission.evaluate("bash", "id").action,
            PermissionV1Action::Allow,
            "a config named by the environment was applied ({host})"
        );
        assert!(services.agents.get("fromenv").is_none(), "{host}");
    }
}

//! The names the shell environment policy learns follow the configs and
//! the catalog as they are read again.
//!
//! The policy is process-wide, so this file holds one test: it is the only
//! one in its binary to install or teach a policy.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;
use std::sync::{Arc, Mutex};

use alforria_core::shell_env;
use alforria_server::engine::EngineSeams;
use alforria_server::ListenOptions;

fn write(path: &Path, value: serde_json::Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_string_pretty(&value).unwrap()).unwrap();
}

/// What the agent's shell is given of a made-up server environment.
fn shell_names() -> Vec<String> {
    let base: Vec<(OsString, OsString)> = [
        "PATH",
        "GITHUB_TOKEN",
        "AWS_PROFILE",
        "MY_CORP_LLM_KEY",
        "my_corp_llm_key",
        "MY_CORP_OTHER",
        "PROJECT_REFERENCED",
        "HOST_GATEWAY_AUTH",
        "HOST_REFERENCED",
        "CATALOG_ONE",
        "CATALOG_TWO",
    ]
    .into_iter()
    .map(|name| (OsString::from(name), OsString::from("value")))
    .collect();
    let mut names: Vec<String> = shell_env::current()
        .shell_env_from(base)
        .into_iter()
        .map(|(name, _)| name.to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn provider(env: &str) -> alforria_core::catalog::Provider {
    alforria_core::catalog::Provider {
        api: None,
        name: "Catalog".to_string(),
        env: vec![env.to_string()],
        id: "catalog".to_string(),
        npm: None,
        models: BTreeMap::new(),
    }
}

/// A standalone server. A project declares a provider of its own that
/// reads its key from `MY_CORP_LLM_KEY` — a name no list or pattern knows —
/// and refers to `{env:PROJECT_REFERENCED}`: both are withheld from the
/// agent's shell once the instance has booted, and released when the
/// project no longer names them and its config is read again. The host
/// config and the catalog are followed the same way. `GITHUB_TOKEN` and
/// `AWS_PROFILE` pass throughout.
#[test]
fn learned_names_follow_a_config_or_catalog_reload() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let paths = alforria_core::GlobalPaths::resolve_with(root.join("home"), |_| None);
    std::fs::create_dir_all(&paths.home).unwrap();
    let host_file = paths.config.join("opencode.json");
    let project_file = repo.join("opencode.json");
    write(&host_file, serde_json::json!({}));
    write(
        &project_file,
        serde_json::json!({
            "username": "{env:PROJECT_REFERENCED}",
            "provider": { "corp": { "env": ["MY_CORP_LLM_KEY"] } }
        }),
    );

    let seams = EngineSeams {
        env: alforria_server::engine::no_ambient_env(),
        ..EngineSeams::default()
    };
    let ctx = alforria_server::production_context(&ListenOptions::default(), paths, seams).unwrap();
    assert_eq!(ctx.policy.config_error, None);
    assert!(!shell_env::current().is_restricted());

    // Before any instance: nothing of the project's is known.
    let everything = [
        "AWS_PROFILE",
        "CATALOG_ONE",
        "CATALOG_TWO",
        "GITHUB_TOKEN",
        "HOST_GATEWAY_AUTH",
        "HOST_REFERENCED",
        "MY_CORP_LLM_KEY",
        "MY_CORP_OTHER",
        "PATH",
        "PROJECT_REFERENCED",
        "my_corp_llm_key",
    ];
    assert_eq!(shell_names(), everything);

    // The instance boots: its project's provider variable and reference
    // are withheld, whatever the case.
    ctx.instances.load(&repo).unwrap();
    let without = |gone: &[&str]| -> Vec<&str> {
        everything
            .iter()
            .copied()
            .filter(|name| !gone.contains(name))
            .collect()
    };
    assert_eq!(
        shell_names(),
        without(&["MY_CORP_LLM_KEY", "my_corp_llm_key", "PROJECT_REFERENCED"])
    );

    // The project swaps its provider for another; the config is read
    // again at the next boot. The old names are released, the new one is
    // withheld.
    write(
        &project_file,
        serde_json::json!({ "provider": { "corp": { "env": ["MY_CORP_OTHER"] } } }),
    );
    ctx.instances.dispose_directory(&repo);
    assert_eq!(shell_names(), everything, "a disposed instance's names go");
    ctx.instances.load(&repo).unwrap();
    assert_eq!(shell_names(), without(&["MY_CORP_OTHER"]));

    // The host config gains a provider and a reference after startup.
    write(
        &host_file,
        serde_json::json!({
            "username": "{env:HOST_REFERENCED}",
            "provider": { "gateway": { "env": ["HOST_GATEWAY_AUTH"] } }
        }),
    );
    write(&project_file, serde_json::json!({}));
    ctx.instances.dispose_directory(&repo);
    ctx.instances.load(&repo).unwrap();
    assert_eq!(
        shell_names(),
        without(&["HOST_GATEWAY_AUTH", "HOST_REFERENCED"])
    );
    // And loses them again.
    write(&host_file, serde_json::json!({}));
    ctx.instances.dispose_directory(&repo);
    ctx.instances.load(&repo).unwrap();
    assert_eq!(shell_names(), everything);

    // The catalog: each read teaches the policy what it names now.
    let providers = Arc::new(Mutex::new(BTreeMap::from([(
        "catalog".to_string(),
        provider("CATALOG_ONE"),
    )])));
    let catalog = alforria_server::state::learning_catalog({
        let providers = providers.clone();
        Arc::new(move || Ok(providers.lock().unwrap().clone()))
    });
    catalog().unwrap();
    assert_eq!(shell_names(), without(&["CATALOG_ONE"]));
    providers
        .lock()
        .unwrap()
        .insert("catalog".to_string(), provider("CATALOG_TWO"));
    catalog().unwrap();
    assert_eq!(shell_names(), without(&["CATALOG_TWO"]));
}

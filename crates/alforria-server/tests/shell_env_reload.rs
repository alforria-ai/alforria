//! The names the shell environment policy learns from the configs and the
//! catalog: learned at every boot and read, and kept until the process
//! exits.
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

fn context(paths: &alforria_core::GlobalPaths) -> Arc<alforria_server::state::ServerContext> {
    let seams = EngineSeams {
        env: alforria_server::engine::no_ambient_env(),
        ..EngineSeams::default()
    };
    let ctx = alforria_server::production_context(&ListenOptions::default(), paths.clone(), seams)
        .unwrap();
    assert_eq!(ctx.policy.config_error, None);
    ctx
}

/// A standalone server. A project declares a provider of its own that
/// reads its key from `MY_CORP_LLM_KEY` — a name no list or pattern knows —
/// and refers to `{env:PROJECT_REFERENCED}`: both are withheld from the
/// agent's shell once the instance has booted, and stay withheld when the
/// instance is disposed (its processes can outlive it) and when the
/// project stops naming them and its config is read again. What a later
/// boot, the host config or the catalog names is added. `shell_env.allow`
/// releases a name; only a restart forgets one. `GITHUB_TOKEN` and
/// `AWS_PROFILE` pass throughout.
#[test]
fn learned_names_are_added_at_every_boot_and_kept_until_restart() {
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

    let ctx = context(&paths);
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
    let without = |gone: &[&str]| -> Vec<&str> {
        everything
            .iter()
            .copied()
            .filter(|name| !gone.contains(name))
            .collect()
    };

    // The instance boots: its project's provider variable and reference
    // are withheld, whatever the case.
    ctx.instances.load(&repo).unwrap();
    let mut gone = vec!["MY_CORP_LLM_KEY", "my_corp_llm_key", "PROJECT_REFERENCED"];
    assert_eq!(shell_names(), without(&gone));

    // Disposing forgets nothing: the instance's turns, terminals and MCP
    // servers can still be starting processes.
    ctx.instances.dispose_directory(&repo);
    assert_eq!(shell_names(), without(&gone), "after a dispose");
    ctx.instances.load(&repo).unwrap();
    ctx.instances.dispose_all();
    assert_eq!(shell_names(), without(&gone), "after disposing all");

    // The project swaps its provider for another; the config is read
    // again at the next boot. The new name is withheld and the old ones
    // stay so.
    write(
        &project_file,
        serde_json::json!({ "provider": { "corp": { "env": ["MY_CORP_OTHER"] } } }),
    );
    ctx.instances.load(&repo).unwrap();
    gone.push("MY_CORP_OTHER");
    assert_eq!(
        shell_names(),
        without(&gone),
        "after the provider is removed"
    );

    // The host config gains a provider and a reference after startup, and
    // the project names nothing any more.
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
    gone.extend(["HOST_GATEWAY_AUTH", "HOST_REFERENCED"]);
    assert_eq!(shell_names(), without(&gone));

    // Boots and disposals of one directory racing each other take
    // nothing out.
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                for _ in 0..5 {
                    ctx.instances.load(&repo).unwrap();
                    ctx.instances.dispose_directory(&repo);
                }
            });
        }
    });
    assert_eq!(shell_names(), without(&gone));

    // The catalog: each read adds what it names now.
    let providers = Arc::new(Mutex::new(BTreeMap::from([(
        "catalog".to_string(),
        provider("CATALOG_ONE"),
    )])));
    let catalog = alforria_server::state::learning_catalog({
        let providers = providers.clone();
        Arc::new(move || Ok(providers.lock().unwrap().clone()))
    });
    catalog().unwrap();
    gone.push("CATALOG_ONE");
    assert_eq!(shell_names(), without(&gone));
    providers
        .lock()
        .unwrap()
        .insert("catalog".to_string(), provider("CATALOG_TWO"));
    catalog().unwrap();
    gone.push("CATALOG_TWO");
    assert_eq!(shell_names(), without(&gone));

    // A restart is a fresh registry: a policy loaded from the configs as
    // they are now knows none of what was learned and since removed.
    write(&host_file, serde_json::json!({}));
    let fresh = alforria_server::ServerPolicy::load(&paths).shell_env;
    for name in &gone {
        assert!(!fresh.denied(name), "{name} is forgotten by a restart");
    }

    // The way out without one: `shell_env.allow` names the variable. (The
    // host config is read at startup, so this is a second server in the
    // same process: it keeps the registry and honours the `allow`.)
    write(
        &host_file,
        serde_json::json!({ "shell_env": { "allow": ["MY_CORP_LLM_KEY"] } }),
    );
    let _allowing = context(&paths);
    gone.retain(|name| *name != "MY_CORP_LLM_KEY");
    assert_eq!(shell_names(), without(&gone));
}

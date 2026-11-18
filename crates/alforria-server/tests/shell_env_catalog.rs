//! What the model catalog has the shell environment policy withhold on a
//! standalone server: the credential-shaped variables of the providers
//! in use — declared or selected in a config, signed in to, keyed from the
//! server's environment, or selected by a session — and never the
//! general-purpose ones (`GITHUB_TOKEN`, `AWS_*`) that `gh` and `aws` read.
//!
//! The policy is process-wide, so this file holds one test: it is the only
//! one in its binary to install or teach a policy.

use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;

use alforria_core::shell_env;
use alforria_server::engine::EngineSeams;
use alforria_server::ListenOptions;
use serde_json::json;

fn write(path: &Path, value: serde_json::Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, serde_json::to_string_pretty(&value).unwrap()).unwrap();
}

const SERVER_ENV: &[&str] = &[
    "ACME_REGION",
    "ACME_TOKEN",
    "AWS_ACCESS_KEY_ID",
    "AWS_REGION",
    "AWS_SECRET_ACCESS_KEY",
    "ENVONLY_REGION",
    "ENVONLY_TOKEN",
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "MISTRAL_API_KEY",
    "NPM_TOKEN",
    "PATH",
    "PICKED_KEY",
    "PICKED_PROJECT",
    "STORED_ENDPOINT",
    "STORED_SECRET",
    "SWITCHED_OFF_TOKEN",
    "UNUSED_TOKEN",
];

/// What the agent's shell is given of a made-up server environment.
fn shell_names() -> Vec<String> {
    let base: Vec<(OsString, OsString)> = SERVER_ENV
        .iter()
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

fn without(gone: &[&str]) -> Vec<&'static str> {
    SERVER_ENV
        .iter()
        .copied()
        .filter(|name| !gone.contains(name))
        .collect()
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

/// A standalone server whose cached catalog lists, as the models.dev one
/// does, GitHub Copilot (`GITHUB_TOKEN`) and Amazon Bedrock (the `AWS_*`
/// access keys and region) among providers nobody here uses. Their
/// variables, and those of every other unused provider, reach the agent's
/// shell: `gh` and `aws` keep working. A provider's catalog variables are
/// withheld once the install uses it — a config declares it or selects
/// its model, or credentials are stored for it — and then only the ones
/// shaped like a credential: a region, a project or an endpoint passes.
/// A provider whose key is in the server's environment alone is in use
/// too, from startup, and a project's `disabled_providers` takes nothing
/// out of the set. `GITHUB_TOKEN` and `AWS_*` pass even when they are set
/// in the server's environment and Copilot and Bedrock are configured;
/// `shell_env.deny` is how a host withholds them.
#[test]
fn the_catalog_withholds_only_the_keys_of_the_providers_in_use() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let paths = alforria_core::GlobalPaths::resolve_with(root.join("home"), |_| None);
    std::fs::create_dir_all(&paths.home).unwrap();
    let host_file = paths.config.join("opencode.json");
    write(&host_file, json!({}));
    let provider =
        |id: &str, env: &[&str]| json!({ "id": id, "name": id, "env": env, "models": {} });
    write(
        &paths.cache.join("models.json"),
        json!({
            "github-copilot": provider("github-copilot", &["GITHUB_TOKEN"]),
            "amazon-bedrock": provider(
                "amazon-bedrock",
                &["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_REGION"],
            ),
            "mistral": provider("mistral", &["MISTRAL_API_KEY"]),
            "acme": provider("acme", &["ACME_TOKEN", "ACME_REGION"]),
            "picked": provider("picked", &["PICKED_KEY", "PICKED_PROJECT"]),
            "stored": provider("stored", &["STORED_SECRET", "STORED_ENDPOINT"]),
            "unused": provider("unused", &["UNUSED_TOKEN"]),
            "envonly": provider("envonly", &["ENVONLY_TOKEN", "ENVONLY_REGION"]),
            "switched-off": provider("switched-off", &["SWITCHED_OFF_TOKEN"]),
        }),
    );
    // The server's real environment: a key for a provider no config
    // names and no credentials are stored for, and the user's own `gh`
    // and `aws` credentials, which Copilot and Bedrock would be keyed
    // from. (This binary holds this one test.)
    std::env::set_var("ENVONLY_TOKEN", "sk-envonly");
    std::env::set_var("GITHUB_TOKEN", "ghp-the-users");
    std::env::set_var("AWS_ACCESS_KEY_ID", "AKIA-the-users");
    let repo = |name: &str, config: serde_json::Value| {
        let repo = root.join(name);
        write(&repo.join("opencode.json"), config);
        repo
    };

    let ctx = context(&paths);
    assert!(!shell_env::current().is_restricted());
    assert_eq!(
        (ctx.catalog)().unwrap().len(),
        10,
        "the catalog fixture and the built-in provider are read"
    );

    // No provider configured: the name shaped like an API key is
    // withheld, as it is without any catalog, and so is the token of the
    // provider keyed from the environment — not its region, and not
    // `GITHUB_TOKEN` or the `AWS_*` keys, set though they are.
    let mut gone = vec!["MISTRAL_API_KEY", "ENVONLY_TOKEN"];
    assert_eq!(shell_names(), without(&gone), "before any instance");
    ctx.instances.load(&repo("plain", json!({}))).unwrap();
    assert_eq!(shell_names(), without(&gone), "no provider configured");

    // A project declares the acme provider: its token is withheld, its
    // region is not.
    ctx.instances
        .load(&repo("acme", json!({ "provider": { "acme": {} } })))
        .unwrap();
    gone.push("ACME_TOKEN");
    assert_eq!(shell_names(), without(&gone), "acme configured");

    // Copilot and Bedrock in use: their variables are the user's `gh` and
    // `aws` credentials first, and still pass.
    ctx.instances
        .load(&repo(
            "cloud",
            json!({ "provider": { "github-copilot": {}, "amazon-bedrock": {} } }),
        ))
        .unwrap();
    assert_eq!(
        shell_names(),
        without(&gone),
        "copilot and bedrock configured"
    );

    // A provider selected as the model, and one with stored credentials.
    ctx.instances
        .load(&repo("picked", json!({ "model": "picked/some-model" })))
        .unwrap();
    gone.push("PICKED_KEY");
    assert_eq!(shell_names(), without(&gone), "a provider's model selected");
    ctx.auth_store
        .set("stored", json!({ "type": "api", "key": "sk-stored" }))
        .unwrap();
    ctx.instances.load(&repo("signed-in", json!({}))).unwrap();
    gone.push("STORED_SECRET");
    assert_eq!(shell_names(), without(&gone), "credentials stored");

    // A project's config cannot take a provider out of the set: one the
    // user signs in to is withheld although the project that is open
    // switches it off.
    ctx.auth_store
        .set("switched-off", json!({ "type": "api", "key": "sk-off" }))
        .unwrap();
    ctx.instances
        .load(&repo(
            "switching-off",
            json!({
                "disabled_providers": ["switched-off", "envonly"],
                "enabled_providers": ["acme"]
            }),
        ))
        .unwrap();
    gone.push("SWITCHED_OFF_TOKEN");
    assert_eq!(shell_names(), without(&gone), "disabled by a project");

    // The restricted posture is as it was: every name the catalog lists
    // is withheld there, used or not, credential or not.
    let restricted = shell_env::current().as_ref().clone().restricted(true);
    for name in [
        "ACME_REGION",
        "ENVONLY_TOKEN",
        "ENVONLY_REGION",
        "UNUSED_TOKEN",
        "GITHUB_TOKEN",
        "AWS_REGION",
    ] {
        assert!(restricted.denied(name), "{name} on a restricted server");
    }

    // A host that signs in to Copilot with `GITHUB_TOKEN` and wants it
    // kept from the agent says so. (The host config is read at startup,
    // so this is a second server in the same process.)
    write(
        &host_file,
        json!({ "shell_env": { "deny": ["GITHUB_TOKEN"] } }),
    );
    let _denying = context(&paths);
    gone.push("GITHUB_TOKEN");
    assert_eq!(shell_names(), without(&gone), "shell_env.deny");
}

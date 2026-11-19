//! The host config a server process applies is the one it started with:
//! an instance booted later does not read the host files again, so a file
//! that broke, vanished or was rewritten in the meantime neither drops the
//! host's permission ceiling nor lifts the project restriction. A
//! developer's own server (nothing unattended) keeps re-reading, and keeps
//! the last config that loaded when the file stops parsing.

mod support;

use std::path::Path;

use alforria_schema::permission_v1::PermissionV1Action;
use axum::http::StatusCode;
use serde_json::{json, Value};

use support::{basic, call, fixture_with, Fixture, Signer};

const PASSWORD: &str = "hunter2";

fn bearer_host(signer: &Signer, extra: Value) -> Value {
    let mut host = json!({
        "server": { "auth": { "bearer": { "keys": [signer.public_key()], "audience": "a" } } },
        "permission": { "bash": "deny" },
    });
    alforria_core::merge_deep(&mut host, &extra);
    host
}

fn host_file(f: &Fixture) -> std::path::PathBuf {
    f.paths.config.join("opencode.json")
}

/// What `permission` on `pattern` resolves to for the `build` agent of the
/// instance booted now for `directory`.
fn build_action(f: &Fixture, directory: &Path, permission: &str) -> PermissionV1Action {
    std::fs::create_dir_all(directory).unwrap();
    let services = f.ctx.instances.load(directory).expect("the instance boots");
    let build = services.agents.get("build").expect("the build agent");
    build.permission.evaluate(permission, "id").action
}

/// The reviewer's scenario: the server runs with `bash: deny`, the global
/// config file stops parsing (or is removed), and a directory is opened
/// for the first time. The ceiling was gone: the built-in `*: allow`.
#[tokio::test]
async fn an_instance_booted_after_the_global_config_broke_keeps_the_startup_ceiling() {
    let signer = Signer::new();
    let f = fixture_with(|_| bearer_host(&signer, json!({})), Some(PASSWORD), vec![]);
    assert_eq!(
        build_action(&f, &f.dirs.repo, "bash"),
        PermissionV1Action::Deny
    );

    std::fs::write(host_file(&f), "{ not json").unwrap();
    assert_eq!(
        build_action(&f, &f.dirs.other, "bash"),
        PermissionV1Action::Deny,
        "an unparseable global config dropped the host's ceiling"
    );

    std::fs::remove_file(host_file(&f)).unwrap();
    assert_eq!(
        build_action(&f, &f.dirs.other.join("third"), "bash"),
        PermissionV1Action::Deny,
        "a removed global config dropped the host's ceiling"
    );
}

/// What decides how far a project's config is believed is read once, with
/// the access policy: rewriting the host file to drop bearer auth, trust
/// projects fully and allow `bash` changes nothing for a later boot.
#[tokio::test]
async fn a_host_file_rewritten_after_startup_does_not_lift_the_restriction() {
    let signer = Signer::new();
    let f = fixture_with(|_| bearer_host(&signer, json!({})), Some(PASSWORD), vec![]);
    std::fs::write(
        host_file(&f),
        json!({
            "server": { "trust": { "project": "full" } },
            "permission": { "bash": "allow" },
        })
        .to_string(),
    )
    .unwrap();
    // A project that brings its own agent and grants: kept on a server
    // that trusts project config, dropped on this one.
    std::fs::write(
        f.dirs.other.join("opencode.json"),
        json!({
            "agent": {
                "mine": { "permission": { "bash": "allow" } },
                "build": { "permission": { "bash": "allow" } },
            },
        })
        .to_string(),
    )
    .unwrap();
    assert_eq!(
        build_action(&f, &f.dirs.other, "bash"),
        PermissionV1Action::Deny
    );
    let services = f.ctx.instances.load(&f.dirs.other).unwrap();
    assert!(
        services.agents.get("mine").is_none(),
        "project config was read as trusted after the host file changed"
    );
}

/// The operator's own way to change the host config of a running server —
/// `PATCH /global/config`, an operator route — still takes effect.
#[tokio::test]
async fn a_global_config_written_through_the_api_reaches_the_next_boot() {
    let signer = Signer::new();
    let f = fixture_with(|_| bearer_host(&signer, json!({})), Some(PASSWORD), vec![]);
    assert_eq!(
        build_action(&f, &f.dirs.repo, "webfetch"),
        PermissionV1Action::Allow
    );
    let response = call(
        &f.router,
        "PATCH",
        "/global/config",
        Some(&basic("alforria", PASSWORD)),
        Some(json!({ "permission": { "webfetch": "deny" } })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        build_action(&f, &f.dirs.repo, "webfetch"),
        PermissionV1Action::Deny
    );
    assert_eq!(
        build_action(&f, &f.dirs.repo, "bash"),
        PermissionV1Action::Deny
    );
}

/// An operator's `PATCH /global/config` applies what the operator sent and
/// nothing else: host files rewritten on disk in the meantime — the one
/// the route writes, and one it does not — are not adopted with it.
#[tokio::test]
async fn an_api_config_write_does_not_adopt_host_files_tampered_on_disk() {
    let signer = Signer::new();
    let f = fixture_with(|_| bearer_host(&signer, json!({})), Some(PASSWORD), vec![]);
    // Out of band, as an agent with a shell would: the global file loses
    // its deny, and `~/.opencode` gains a full-trust switch and a grant.
    std::fs::write(host_file(&f), json!({ "model": "acme/m" }).to_string()).unwrap();
    let dot = f.paths.home.join(".opencode");
    std::fs::create_dir_all(&dot).unwrap();
    std::fs::write(
        dot.join("opencode.json"),
        json!({
            "server": { "trust": { "project": "full" } },
            "permission": { "bash": "allow" },
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        f.dirs.other.join("opencode.json"),
        json!({ "agent": { "mine": { "permission": { "bash": "allow" } } } }).to_string(),
    )
    .unwrap();

    let response = call(
        &f.router,
        "PATCH",
        "/global/config",
        Some(&basic("alforria", PASSWORD)),
        Some(json!({ "permission": { "webfetch": "deny" } })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);

    assert_eq!(
        build_action(&f, &f.dirs.other, "bash"),
        PermissionV1Action::Deny,
        "the config write adopted a tampered host file"
    );
    assert_eq!(
        build_action(&f, &f.dirs.other, "webfetch"),
        PermissionV1Action::Deny,
        "the operator's own change did not apply"
    );
    let services = f.ctx.instances.load(&f.dirs.other).unwrap();
    assert!(
        services.agents.get("mine").is_none(),
        "project config was read as trusted after the config write"
    );
}

/// A developer's own server reads its config again at each boot, as
/// before — and when the file stops parsing, the last config that loaded
/// stands instead of none.
#[tokio::test]
async fn a_standalone_server_rereads_and_keeps_the_last_good_config_on_a_parse_error() {
    let f = fixture_with(
        |_| json!({ "permission": { "bash": "deny" } }),
        None,
        vec![],
    );
    assert_eq!(
        build_action(&f, &f.dirs.repo, "bash"),
        PermissionV1Action::Deny
    );

    std::fs::write(host_file(&f), "{ not json").unwrap();
    assert_eq!(
        build_action(&f, &f.dirs.other, "bash"),
        PermissionV1Action::Deny,
        "a parse error dropped the ceiling"
    );

    // A valid edit is picked up without a restart.
    std::fs::write(
        host_file(&f),
        json!({ "permission": { "bash": "ask" } }).to_string(),
    )
    .unwrap();
    assert_eq!(
        build_action(&f, &f.dirs.other.join("third"), "bash"),
        PermissionV1Action::Ask
    );
}

/// A host source that does not parse at startup: the policy carries the
/// error (so `listen` refuses), and a directory opened anyway — the
/// in-process server has no listener to refuse — fails with it instead of
/// booting under no ceiling at all.
#[tokio::test]
async fn a_host_source_that_does_not_load_at_startup_boots_no_instance() {
    for broken in ["global", "dot-opencode"] {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let (home, repo) = (root.join("home"), root.join("repo"));
        std::fs::create_dir_all(&repo).unwrap();
        let paths = alforria_core::GlobalPaths::resolve_with(home.clone(), |_| None);
        std::fs::create_dir_all(&paths.config).unwrap();
        let good = json!({ "permission": { "bash": "deny" } }).to_string();
        let (global, dot) = match broken {
            "global" => ("{ not json".to_string(), good),
            _ => (good, "{ not json".to_string()),
        };
        std::fs::write(paths.config.join("opencode.json"), global).unwrap();
        std::fs::create_dir_all(home.join(".opencode")).unwrap();
        std::fs::write(home.join(".opencode").join("opencode.json"), dot).unwrap();

        let seams = alforria_server::engine::EngineSeams {
            env: alforria_server::engine::no_ambient_env(),
            ..Default::default()
        };
        let ctx = alforria_server::production_context(
            &alforria_server::ListenOptions::default(),
            paths,
            seams,
        )
        .unwrap();
        assert!(ctx.policy.config_error.is_some(), "{broken}");
        assert!(
            ctx.instances.load(&repo).is_err(),
            "{broken}: an instance booted without the host config"
        );
    }
}

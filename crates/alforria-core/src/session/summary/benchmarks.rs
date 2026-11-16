//! Manual scaling check that awaits the processor's real background summary.

use super::*;
use crate::session::llm::StreamInput;
use crate::session::processor::{Processor, ProcessorDeps, ProcessorInput};
use crate::session::snapshot::DisabledSnapshot;
use crate::session::test_support::{
    assistant_message, compaction_part, create_session, harness, test_model, text_stream,
    user_message, AllowAll,
};
use alforria_llm::schema::events::LlmEvent;
use alforria_llm::schema::messages::Message;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

struct ObservedSummary {
    inner: Arc<SessionSummary>,
    completed: mpsc::UnboundedSender<Result<(), SessionError>>,
}

impl SummarySummarize for ObservedSummary {
    fn summarize(&self, session_id: &str, message_id: &str) -> BoxFuture<'static, ()> {
        let inner = self.inner.clone();
        let completed = self.completed.clone();
        let (session_id, message_id) = (session_id.to_owned(), message_id.to_owned());
        Box::pin(async move {
            completed
                .send(inner.summarize(&session_id, &message_id).await)
                .unwrap();
        })
    }

    fn reset<'a>(&'a self, session_id: &'a str) -> BoxFuture<'a, Result<(), SessionError>> {
        Box::pin(self.inner.reset(session_id))
    }

    fn attach_diffs<'a>(
        &'a self,
        session_id: &'a str,
        message_id: &'a str,
    ) -> BoxFuture<'static, ()> {
        SummarySummarize::attach_diffs(self.inner.as_ref(), session_id, message_id)
    }
}

#[tokio::test]
#[ignore = "manual benchmark: cargo test -p alforria-core benchmark_completed_step --lib -- --ignored --nocapture"]
async fn benchmark_completed_step_with_background_summary() {
    const SAMPLES: usize = 10;
    for archived in [100, 1000, 10_000] {
        let mut script = text_stream("hello");
        script.insert(0, Ok(LlmEvent::StepStart { index: 0.0 }));
        let h = harness("completed-step-benchmark", vec![script; SAMPLES]);
        let session = create_session(&h.services.sessions, &h.worktree);
        for index in 0..archived {
            h.services
                .sessions
                .update_message(&user_message(
                    &session.id,
                    &format!("msg_archived_{index:05}"),
                    index as f64,
                ))
                .unwrap();
        }
        h.services
            .sessions
            .update_message(&user_message(&session.id, "msg_compact", archived as f64))
            .unwrap();
        h.services
            .sessions
            .update_part(&compaction_part(
                &session.id,
                "msg_compact",
                "prt_compact",
                true,
                None,
                None,
            ))
            .unwrap();
        h.services
            .sessions
            .update_message(&assistant_message(
                &session.id,
                "msg_compacted",
                "msg_compact",
                archived + 1,
                Some(true),
                Some("stop".into()),
            ))
            .unwrap();
        let mut user = user_message(&session.id, "msg_u", (archived + 2) as f64);
        if let V1Message::User { summary, .. } = &mut user {
            *summary = Some(alforria_schema::session_v1::V1Summary {
                title: None,
                body: None,
                diffs: vec![],
            });
        }
        h.services.sessions.update_message(&user).unwrap();
        let (completed, mut completion) = mpsc::unbounded_channel();
        let snapshot: Arc<dyn Snapshot> = Arc::new(DisabledSnapshot);
        let config = Arc::new(h.config.clone());
        let summary = Arc::new(ObservedSummary {
            inner: Arc::new(SessionSummary::new(SummaryDeps {
                sessions: h.services.sessions.clone(),
                snapshot: snapshot.clone(),
                events: h.services.events.clone(),
                config: config.clone(),
            })),
            completed,
        });
        let model = test_model();
        let mut elapsed = Vec::new();
        for _ in 0..SAMPLES {
            // Keep both the active context and the current turn fixed while
            // only the archived history changes between benchmark cases.
            h.services.storage.with_connection(|conn| {
                conn.execute("DELETE FROM part WHERE message_id = 'msg_a'", [])
                    .unwrap();
            });
            let assistant =
                assistant_message(&session.id, "msg_a", "msg_u", archived + 3, None, None);
            h.services.sessions.update_message(&assistant).unwrap();
            let start = std::time::Instant::now();
            let context = h.services.messages.active_context(&session.id).unwrap();
            assert_eq!(context.len(), 4);
            std::hint::black_box(context);
            let processor = Processor::create(
                ProcessorDeps {
                    sessions: h.services.sessions.clone(),
                    messages: h.services.messages.clone(),
                    status: h.services.status.clone(),
                    events: h.services.events.clone(),
                    snapshot: snapshot.clone(),
                    agents: h.services.agents.clone(),
                    config: config.clone(),
                    llm: h.llm.clone(),
                    permission: Arc::new(AllowAll),
                    summary: summary.clone(),
                    clock: h.services.clock(),
                },
                ProcessorInput {
                    assistant_message: assistant,
                    session_id: session.id.clone(),
                    model: model.processor_model(),
                },
            )
            .await;
            processor
                .process(
                    StreamInput {
                        user: user.clone(),
                        session_id: session.id.clone(),
                        parent_session_id: None,
                        project_id: None,
                        client: "cli".into(),
                        model: model.llm.clone(),
                        agent: h.services.agents.get("build").unwrap().clone(),
                        permission: session.permission.clone(),
                        system: vec![],
                        messages: vec![Message::user("hi")],
                        small: false,
                        tools: vec![],
                        retries: None,
                        tool_choice: None,
                    },
                    CancellationToken::new(),
                )
                .await
                .unwrap();
            // Include all work in the per-step summary task, not just the
            // foreground processor's return time.
            tokio::time::timeout(std::time::Duration::from_secs(5), completion.recv())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            elapsed.push(start.elapsed().as_micros());
        }
        elapsed.sort_unstable();
        println!(
            "archived={archived}, active_messages=4, completed_step_and_summary_median_us={}",
            elapsed[SAMPLES / 2]
        );
    }
}

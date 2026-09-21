//! Question service — port of `question/index.ts` (27-157): ask/reply/reject
//! flow over a pending-request registry keyed by `que_` id (the
//! `plan_exit` + `question` tool backend).
//!
//! [`QuestionService::ask`] publishes `question.asked` and awaits the answer
//! oneshot (spec §7 decision 6: the Effect `Deferred` of
//! question/index.ts:37-44 becomes a `tokio::sync::oneshot`);
//! [`QuestionService::reply`] publishes `question.replied` (answers mapped
//! `[...a]`, 129) and resolves; [`QuestionService::reject`] publishes
//! `question.rejected` and fails with `RejectedError` (message
//! `The user dismissed this question`, question/index.ts:27-31). Unknown ids
//! fail with `Question.NotFoundError` (33-35). All three events are
//! ephemeral in TS (`define` without `durable`,
//! schema-src/v1/question.ts:58-66).

use std::sync::{Arc, Mutex, MutexGuard};

use opencode_schema::question_v1::{
    QuestionAskedData, QuestionRejectedData, QuestionRepliedData, QuestionV1Answer, QuestionV1Info,
    QuestionV1Request, QuestionV1Tool,
};
use tokio::sync::oneshot;

use crate::event::bus::{EventBus, PublishOptions};
use crate::event::definition::Definition;
use crate::session::ids;
use crate::tool::def::BoxFuture;
use crate::tool::error::ToolError;

/// `Question.Event.Asked` (schema-src/v1/question.ts:58) — ephemeral.
pub const QUESTION_ASKED: Definition = Definition::ephemeral("question.asked");

/// `Question.Event.Replied` (schema-src/v1/question.ts:59) — ephemeral.
pub const QUESTION_REPLIED: Definition = Definition::ephemeral("question.replied");

/// `Question.Event.Rejected` (schema-src/v1/question.ts:60) — ephemeral.
pub const QUESTION_REJECTED: Definition = Definition::ephemeral("question.rejected");

/// The failure surface of the question service:
/// `Question.RejectedError` (question/index.ts:27-31) and
/// `Question.NotFoundError` (33-35).
#[derive(Debug, Clone, PartialEq)]
pub enum QuestionError {
    /// `Question.RejectedError`.
    Rejected,
    /// `Question.NotFoundError`.
    NotFound { request_id: String },
    /// An event-bus defect (TS: a failing `events.publish` effect).
    Publish(String),
}

impl std::fmt::Display for QuestionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QuestionError::Rejected => f.write_str("The user dismissed this question"),
            QuestionError::NotFound { request_id } => {
                write!(f, "Question.NotFoundError: requestID: {request_id}")
            }
            QuestionError::Publish(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for QuestionError {}

/// `Question.Service.ask` input (question/index.ts:49-53).
pub struct QuestionAskInput {
    pub session_id: String,
    pub questions: Vec<QuestionV1Info>,
    pub tool: Option<QuestionV1Tool>,
}

/// `PendingEntry` (question/index.ts:37-40).
struct PendingEntry {
    info: QuestionV1Request,
    done: oneshot::Sender<Result<Vec<QuestionV1Answer>, QuestionError>>,
}

/// `Question.Service` (question/index.ts:48-61).
pub struct QuestionService {
    events: Arc<EventBus>,
    pending: Mutex<Vec<PendingEntry>>,
}

/// Removes the pending entry when the ask future is dropped
/// (question/index.ts:106-112 — `Effect.ensuring(pending.delete(id))`).
struct PendingCleanup<'a> {
    service: &'a QuestionService,
    id: String,
}

impl Drop for PendingCleanup<'_> {
    fn drop(&mut self) {
        self.service
            .lock_pending()
            .retain(|entry| entry.info.id != self.id);
    }
}

impl QuestionService {
    pub fn new(events: Arc<EventBus>) -> QuestionService {
        QuestionService {
            events,
            pending: Mutex::new(Vec::new()),
        }
    }

    fn lock_pending(&self) -> MutexGuard<'_, Vec<PendingEntry>> {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn publish(
        &self,
        definition: &Definition,
        data: serde_json::Value,
    ) -> Result<(), QuestionError> {
        self.events
            .publish(definition, data, PublishOptions::default())
            .map(|_| ())
            .map_err(|err| QuestionError::Publish(err.to_string()))
    }

    /// `list` (question/index.ts:150-153): pending requests in insertion
    /// order.
    pub fn list(&self) -> Vec<QuestionV1Request> {
        self.lock_pending()
            .iter()
            .map(|entry| entry.info.clone())
            .collect()
    }

    /// `ask` (question/index.ts:87-112 — binding): register, publish
    /// `question.asked`, await the answer.
    pub async fn ask(
        &self,
        input: QuestionAskInput,
    ) -> Result<Vec<QuestionV1Answer>, QuestionError> {
        let info = QuestionV1Request {
            id: ids::QuestionId::ascending(None).expect("generates que_ id"),
            session_id: input.session_id.clone(),
            questions: input.questions.clone(),
            tool: input.tool.clone(),
        };
        let (done, rx) = oneshot::channel();
        {
            let mut pending = self.lock_pending();
            pending.retain(|entry| entry.info.id != info.id);
            pending.push(PendingEntry {
                info: info.clone(),
                done,
            });
        }
        let data = serde_json::to_value(QuestionAskedData {
            id: info.id.clone(),
            session_id: info.session_id.clone(),
            questions: info.questions.clone(),
            tool: info.tool.clone(),
        })
        .map_err(|err| QuestionError::Publish(err.to_string()))?;
        self.publish(&QUESTION_ASKED, data)?;

        let guard = PendingCleanup {
            service: self,
            id: info.id.clone(),
        };
        let result = match rx.await {
            Ok(result) => result,
            // The service went away with the ask still pending — the TS
            // finalizer fails every pending deferred with RejectedError
            // (question/index.ts:74-81).
            Err(_) => Err(QuestionError::Rejected),
        };
        drop(guard);
        result
    }

    /// `reply` (question/index.ts:114-132 — binding): publishes
    /// `question.replied` and resolves the ask with the answers.
    pub fn reply(
        &self,
        request_id: &str,
        answers: Vec<QuestionV1Answer>,
    ) -> Result<(), QuestionError> {
        let mut pending = self.lock_pending();
        let position = pending
            .iter()
            .position(|entry| entry.info.id == request_id)
            .ok_or_else(|| QuestionError::NotFound {
                request_id: request_id.to_string(),
            })?;
        let existing = pending.remove(position);
        drop(pending);
        let data = serde_json::to_value(QuestionRepliedData {
            session_id: existing.info.session_id.clone(),
            request_id: existing.info.id.clone(),
            answers: answers.clone(),
        })
        .map_err(|err| QuestionError::Publish(err.to_string()))?;
        self.publish(&QUESTION_REPLIED, data)?;
        let _ = existing.done.send(Ok(answers));
        Ok(())
    }

    /// `reject` (question/index.ts:134-148 — binding): publishes
    /// `question.rejected` and fails the ask with `RejectedError`.
    pub fn reject(&self, request_id: &str) -> Result<(), QuestionError> {
        let mut pending = self.lock_pending();
        let position = pending
            .iter()
            .position(|entry| entry.info.id == request_id)
            .ok_or_else(|| QuestionError::NotFound {
                request_id: request_id.to_string(),
            })?;
        let existing = pending.remove(position);
        drop(pending);
        let data = serde_json::to_value(QuestionRejectedData {
            session_id: existing.info.session_id.clone(),
            request_id: existing.info.id.clone(),
        })
        .map_err(|err| QuestionError::Publish(err.to_string()))?;
        self.publish(&QUESTION_REJECTED, data)?;
        let _ = existing.done.send(Err(QuestionError::Rejected));
        Ok(())
    }
}

/// The M4 `Question` seam (tool/question.rs:44) against this service:
/// prompts convert to infos (`custom: None`), a rejection surfaces as the
/// `RejectedError` message (the tool's `Effect.orDie` wraps it either way).
impl crate::tool::question::Question for QuestionService {
    fn ask<'a>(
        &'a self,
        input: crate::tool::question::QuestionAsk<'a>,
    ) -> BoxFuture<'a, Result<Vec<crate::tool::question::QuestionAnswer>, ToolError>> {
        Box::pin(async move {
            let questions: Vec<QuestionV1Info> = input
                .questions
                .iter()
                .map(|prompt| QuestionV1Info {
                    question: prompt.question.clone(),
                    header: prompt.header.clone(),
                    options: prompt.options.clone(),
                    multiple: prompt.multiple,
                    custom: None,
                })
                .collect();
            match self
                .ask(QuestionAskInput {
                    session_id: input.session_id.to_string(),
                    questions,
                    tool: input.tool.clone(),
                })
                .await
            {
                Ok(answers) => Ok(answers),
                Err(QuestionError::Rejected) => Err(crate::tool::question::rejected()),
                Err(other) => Err(ToolError::Failed(other.to_string())),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::broadcast;

    use super::*;
    use crate::event::definition::Payload;

    fn bus() -> Arc<EventBus> {
        Arc::new(EventBus::new(
            crate::storage::Storage::open_in_memory().unwrap(),
            None,
        ))
    }

    fn question() -> QuestionV1Info {
        QuestionV1Info {
            question: "What would you like to do?".to_string(),
            header: "Action".to_string(),
            options: vec![
                opencode_schema::question_v1::QuestionV1Option {
                    label: "Option 1".to_string(),
                    description: "First option".to_string(),
                },
                opencode_schema::question_v1::QuestionV1Option {
                    label: "Option 2".to_string(),
                    description: "Second option".to_string(),
                },
            ],
            multiple: None,
            custom: None,
        }
    }

    fn ask_input() -> QuestionAskInput {
        QuestionAskInput {
            session_id: "ses_test".to_string(),
            questions: vec![question()],
            tool: None,
        }
    }

    async fn recv(rx: &mut broadcast::Receiver<Payload>, type_: &str) -> Payload {
        loop {
            let event = rx.recv().await.unwrap();
            if event.r#type == type_ {
                return event;
            }
        }
    }

    #[tokio::test]
    async fn ask_remains_pending_until_answered() {
        let events = bus();
        let service = Arc::new(QuestionService::new(Arc::clone(&events)));
        let mut asked = events.subscribe("question.asked");
        let handle = {
            let service = Arc::clone(&service);
            let input = ask_input();
            tokio::spawn(async move { service.ask(input).await })
        };

        let event = recv(&mut asked, "question.asked").await;
        assert_eq!(event.data["sessionID"], "ses_test");
        let pending = service.list();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].questions, vec![question()]);

        // Reject-all so the dangling ask settles.
        service.reject(&pending[0].id).unwrap();
        let error = handle.await.unwrap().unwrap_err();
        assert_eq!(error, QuestionError::Rejected);
        assert_eq!(error.to_string(), "The user dismissed this question");
    }

    #[tokio::test]
    async fn reply_resolves_the_pending_ask_with_answers() {
        let events = bus();
        let service = Arc::new(QuestionService::new(Arc::clone(&events)));
        let mut asked = events.subscribe("question.asked");
        let mut replied = events.subscribe("question.replied");
        let handle = {
            let service = Arc::clone(&service);
            let input = ask_input();
            tokio::spawn(async move { service.ask(input).await })
        };
        recv(&mut asked, "question.asked").await;

        let request_id = service.list()[0].id.clone();
        service
            .reply(&request_id, vec![vec!["Option 1".to_string()]])
            .unwrap();
        assert_eq!(
            handle.await.unwrap().unwrap(),
            vec![vec!["Option 1".to_string()]]
        );
        assert!(service.list().is_empty(), "reply removes from pending");

        let event = recv(&mut replied, "question.replied").await;
        assert_eq!(event.data["requestID"], serde_json::json!(request_id));
        assert_eq!(event.data["answers"], serde_json::json!([["Option 1"]]));
    }

    #[tokio::test]
    async fn reject_removes_from_pending_and_fails_the_ask() {
        let events = bus();
        let service = Arc::new(QuestionService::new(Arc::clone(&events)));
        let mut asked = events.subscribe("question.asked");
        let mut rejected = events.subscribe("question.rejected");
        let handle = {
            let service = Arc::clone(&service);
            let input = ask_input();
            tokio::spawn(async move { service.ask(input).await })
        };
        recv(&mut asked, "question.asked").await;

        let request_id = service.list()[0].id.clone();
        service.reject(&request_id).unwrap();
        assert_eq!(handle.await.unwrap().unwrap_err(), QuestionError::Rejected);
        assert!(service.list().is_empty());

        let event = recv(&mut rejected, "question.rejected").await;
        assert_eq!(
            event.data,
            serde_json::json!({ "sessionID": "ses_test", "requestID": request_id })
        );
    }

    #[test]
    fn reply_fails_for_unknown_request_id() {
        let service = QuestionService::new(bus());
        let error = service
            .reply("que_unknown", vec![vec!["Option 1".to_string()]])
            .unwrap_err();
        assert_eq!(
            error,
            QuestionError::NotFound {
                request_id: "que_unknown".to_string()
            }
        );
        assert_eq!(
            error.to_string(),
            "Question.NotFoundError: requestID: que_unknown"
        );
    }

    #[test]
    fn reject_fails_for_unknown_request_id() {
        let service = QuestionService::new(bus());
        assert_eq!(
            service.reject("que_unknown").unwrap_err(),
            QuestionError::NotFound {
                request_id: "que_unknown".to_string()
            }
        );
    }

    #[tokio::test]
    async fn ids_are_que_prefixed_and_unique() {
        let events = bus();
        let service = Arc::new(QuestionService::new(Arc::clone(&events)));
        let mut asked = events.subscribe("question.asked");
        let a = {
            let service = Arc::clone(&service);
            let input = ask_input();
            tokio::spawn(async move { service.ask(input).await })
        };
        recv(&mut asked, "question.asked").await;
        let b = {
            let service = Arc::clone(&service);
            let input = ask_input();
            tokio::spawn(async move { service.ask(input).await })
        };
        recv(&mut asked, "question.asked").await;

        let ids: Vec<String> = service.list().iter().map(|item| item.id.clone()).collect();
        assert_eq!(ids.len(), 2);
        for id in &ids {
            assert!(id.starts_with("que_"), "{id}");
        }
        assert_ne!(ids[0], ids[1]);
        service.reject(&ids[0]).unwrap();
        service.reject(&ids[1]).unwrap();
        let _ = a.await.unwrap();
        let _ = b.await.unwrap();
    }

    #[tokio::test]
    async fn m4_question_trait_round_trip_against_the_real_service() {
        let events = bus();
        let service = Arc::new(QuestionService::new(Arc::clone(&events)));
        let mut asked = events.subscribe("question.asked");

        let handle = {
            let service = Arc::clone(&service);
            tokio::spawn(async move {
                crate::tool::question::Question::ask(
                    &*service,
                    crate::tool::question::QuestionAsk {
                        session_id: "ses_1",
                        questions: &[opencode_schema::question_v1::QuestionV1Prompt {
                            question: "Proceed?".to_string(),
                            header: "Confirm".to_string(),
                            options: vec![opencode_schema::question_v1::QuestionV1Option {
                                label: "yes".to_string(),
                                description: "Proceed".to_string(),
                            }],
                            multiple: None,
                        }],
                        tool: Some(opencode_schema::question_v1::QuestionV1Tool {
                            message_id: "msg_1".to_string(),
                            call_id: "cal_1".to_string(),
                        }),
                    },
                )
                .await
            })
        };
        recv(&mut asked, "question.asked").await;

        let request = service.list().into_iter().next().unwrap();
        service
            .reply(&request.id, vec![vec!["yes".to_string()]])
            .unwrap();
        let answers = handle.await.unwrap().unwrap();
        assert_eq!(answers, vec![vec!["yes".to_string()]]);

        // A rejection surfaces as the dismissed-question tool error.
        let handle = {
            let service = Arc::clone(&service);
            tokio::spawn(async move {
                crate::tool::question::Question::ask(
                    &*service,
                    crate::tool::question::QuestionAsk {
                        session_id: "ses_1",
                        questions: &[opencode_schema::question_v1::QuestionV1Prompt {
                            question: "Proceed?".to_string(),
                            header: "Confirm".to_string(),
                            options: vec![],
                            multiple: None,
                        }],
                        tool: None,
                    },
                )
                .await
            })
        };
        recv(&mut asked, "question.asked").await;
        let request = service.list().into_iter().next().unwrap();
        service.reject(&request.id).unwrap();
        let ToolError::Rejected(message) = handle.await.unwrap().unwrap_err() else {
            panic!("expected a rejected tool error");
        };
        assert_eq!(message, "The user dismissed this question");
    }
}

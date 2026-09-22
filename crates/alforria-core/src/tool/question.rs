//! `question` tool — port of `tool/question.ts` + `question/index.ts`
//! (spec M4.6).
//!
//! [`Question`] is the `Question.Service` seam (question/index.ts:48-62): the
//! session loop (M5) implements the pending-request bookkeeping; M4 provides
//! the auto-answering [`FakeQuestion`] for tests. Rejections carry the
//! `RejectedError` message (question/index.ts:27-31) as a
//! [`ToolError::Failed`] — the tool's `Effect.orDie` wraps them into defects
//! either way.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::{json, Value};

use alforria_schema::question_v1::{QuestionV1Prompt, QuestionV1Tool};

use crate::tool::def::{define, Agents, BoxFuture, ExecuteResult, ToolCtxRef, ToolDef};
use crate::tool::error::ToolError;
use crate::tool::truncate::Truncate;

/// `Question.RejectedError.message` (question/index.ts:27-31).
pub const REJECTED_MESSAGE: &str = "The user dismissed this question";

/// The `ToolError` a rejected question surfaces as. TS dies the tool with
/// the `Question.RejectedError` instance so the processor's `instanceof`
/// check blocks the loop (processor.ts:200-201) — the `Rejected` variant
/// carries exactly that.
pub fn rejected() -> ToolError {
    ToolError::Rejected(REJECTED_MESSAGE.to_string())
}

/// `QuestionV1.Answer` — an array of selected labels.
pub type QuestionAnswer = Vec<String>;

/// The `Question.Service.ask` input shape (question/index.ts:48-60):
/// `{ sessionID, questions, tool? }`.
pub struct QuestionAsk<'a> {
    pub session_id: &'a str,
    pub questions: &'a [QuestionV1Prompt],
    /// `{ messageID, callID }` when the question comes from a tool call.
    pub tool: Option<QuestionV1Tool>,
}

/// `Question.Service` seam (question/index.ts:48-62).
pub trait Question: Send + Sync {
    /// Ask a question; rejection produces [`rejected`] and other failures
    /// die with their message.
    fn ask<'a>(
        &'a self,
        input: QuestionAsk<'a>,
    ) -> BoxFuture<'a, Result<Vec<QuestionAnswer>, ToolError>>;
}

#[derive(Debug, Deserialize)]
pub struct QuestionParameters {
    pub questions: Vec<QuestionV1Prompt>,
}

/// Hand-authored JSON Schema (byte-matches the Effect-generated schema; see
/// `tests/golden/toolschema/question.json`).
pub fn parameters() -> Value {
    json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "properties": {
            "questions": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "question": {
                            "type": "string",
                            "description": "Complete question"
                        },
                        "header": {
                            "type": "string",
                            "description": "Very short label (max 30 chars)"
                        },
                        "options": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "label": {
                                        "type": "string",
                                        "description": "Display text (1-5 words, concise)"
                                    },
                                    "description": {
                                        "type": "string",
                                        "description": "Explanation of choice"
                                    }
                                },
                                "required": [
                                    "label",
                                    "description"
                                ]
                            },
                            "description": "Available choices"
                        },
                        "multiple": {
                            "type": "boolean",
                            "description": "Allow selecting multiple choices"
                        }
                    },
                    "required": [
                        "question",
                        "header",
                        "options"
                    ]
                },
                "description": "Questions to ask"
            }
        },
        "required": [
            "questions"
        ]
    })
}

/// Build the `question` tool.
pub fn question_tool(
    truncate: Arc<dyn Truncate>,
    agents: Arc<dyn Agents>,
    question: Arc<dyn Question>,
) -> ToolDef {
    define(
        "question",
        include_str!("txt/question.txt"),
        parameters(),
        None,
        truncate,
        agents,
        move |params: QuestionParameters, ctx: ToolCtxRef<'_>| {
            run(params, ctx, Arc::clone(&question))
        },
    )
}

fn run(
    params: QuestionParameters,
    ctx: ToolCtxRef<'_>,
    question: Arc<dyn Question>,
) -> BoxFuture<'_, Result<ExecuteResult, ToolError>> {
    Box::pin(async move {
        let tool = ctx.call_id.map(|call_id| QuestionV1Tool {
            message_id: ctx.message_id.to_string(),
            call_id: call_id.to_string(),
        });
        let answers = question
            .ask(QuestionAsk {
                session_id: ctx.session_id,
                questions: &params.questions,
                tool,
            })
            .await?;

        let formatted = params
            .questions
            .iter()
            .enumerate()
            .map(|(i, q)| {
                let answer = answers
                    .get(i)
                    .filter(|answer| !answer.is_empty())
                    .map(|answer| answer.join(", "))
                    .unwrap_or_else(|| "Unanswered".to_string());
                format!("\"{}\"=\"{}\"", q.question, answer)
            })
            .collect::<Vec<_>>()
            .join(", ");

        Ok(ExecuteResult {
            title: format!(
                "Asked {} question{}",
                params.questions.len(),
                if params.questions.len() > 1 { "s" } else { "" }
            ),
            output: format!(
                "User has answered your questions: {formatted}. You can now continue with the user's answers in mind."
            ),
            metadata: json!({ "answers": answers }),
            attachments: None,
        })
    })
}

// ---------------------------------------------------------------------------
// Test fake
// ---------------------------------------------------------------------------

/// A recorded [`QuestionAsk`] (the borrow-free mirror).
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedAsk {
    pub session_id: String,
    pub questions: Vec<QuestionV1Prompt>,
    pub tool: Option<QuestionV1Tool>,
}

/// M4 test fake: auto-answers from a canned queue, records every ask.
/// An exhausted queue (or `reject_all`) produces [`rejected`].
#[derive(Default)]
pub struct FakeQuestion {
    /// Every ask the fake has seen, in order.
    pub asks: Mutex<Vec<RecordedAsk>>,
    /// Canned answers, popped (FIFO) per ask.
    pub queue: Mutex<VecDeque<Vec<QuestionAnswer>>>,
    /// Reject every ask with [`rejected`].
    pub reject_all: bool,
}

impl FakeQuestion {
    /// Queue one answer set for the next ask.
    pub fn enqueue(&self, answers: Vec<QuestionAnswer>) {
        self.queue.lock().unwrap().push_back(answers);
    }
}

impl Question for FakeQuestion {
    fn ask<'a>(
        &'a self,
        input: QuestionAsk<'a>,
    ) -> BoxFuture<'a, Result<Vec<QuestionAnswer>, ToolError>> {
        Box::pin(async move {
            self.asks.lock().unwrap().push(RecordedAsk {
                session_id: input.session_id.to_string(),
                questions: input.questions.to_vec(),
                tool: input.tool.clone(),
            });
            if self.reject_all {
                return Err(rejected());
            }
            self.queue
                .lock()
                .unwrap()
                .pop_front()
                .map(Ok)
                .unwrap_or(Err(rejected()))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::test_support::TempDir;
    use crate::tool::def::Extra;
    use crate::tool::ripgrep::test_support::{fixed_agents, instance, RecordingAsk};
    use crate::tool::truncate::TruncateService;
    use alforria_schema::question_v1::QuestionV1Tool;
    use serde_json::json;

    fn tool(question: Arc<dyn Question>, dir: &std::path::Path) -> ToolDef {
        question_tool(
            Arc::new(TruncateService::default_limits(dir.join("tool-output"))),
            fixed_agents(),
            question,
        )
    }

    fn ctx<'a>(
        ask: &'a RecordingAsk,
        inst: &'a crate::tool::def::InstanceContext,
        extra: &'a Extra,
        call_id: Option<&'a str>,
    ) -> crate::tool::def::ToolCtxRef<'a> {
        crate::tool::def::ToolCtxRef {
            session_id: "ses_1",
            message_id: "msg_1",
            agent: "build",
            call_id,
            abort: tokio_util::sync::CancellationToken::new(),
            messages: &[],
            extra,
            instance: inst,
            ask,
            metadata: ask,
        }
    }

    #[test]
    fn golden_parameters_schema() {
        let golden = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/golden/toolschema/question.json"
        ))
        .unwrap();
        let golden: Value = serde_json::from_str(&golden).unwrap();
        assert_eq!(parameters(), golden);
    }

    #[tokio::test]
    async fn formats_answers_and_records_the_ask_shape() {
        let temp = TempDir::new("question-roundtrip");
        let fake = Arc::new(FakeQuestion::default());
        fake.enqueue(vec![vec!["yes".to_string()], vec![]]);
        let def = tool(Arc::clone(&fake) as Arc<dyn Question>, temp.path());
        let ask = RecordingAsk::new();
        let inst = instance(temp.path());
        let extra = Extra::default();
        let ctx = ctx(&ask, &inst, &extra, Some("cal_1"));

        let result = (def.execute)(
            json!({ "questions": [
                { "question": "Proceed?", "header": "Confirm",
                  "options": [{ "label": "yes", "description": "Proceed" }] },
                { "question": "Which one?", "header": "Pick",
                  "options": [{ "label": "a", "description": "A" }] },
            ] }),
            ctx,
        )
        .await
        .unwrap();

        assert_eq!(result.title, "Asked 2 questions");
        assert_eq!(
            result.output,
            "User has answered your questions: \"Proceed?\"=\"yes\", \"Which one?\"=\"Unanswered\". You can now continue with the user's answers in mind."
        );
        assert_eq!(
            result.metadata,
            json!({ "answers": [["yes"], []], "truncated": false })
        );

        // The ask shape carries the session id and the tool call reference.
        let asks = fake.asks.lock().unwrap();
        assert_eq!(asks.len(), 1);
        assert_eq!(asks[0].session_id, "ses_1");
        assert_eq!(asks[0].questions.len(), 2);
        assert_eq!(
            asks[0].tool,
            Some(QuestionV1Tool {
                message_id: "msg_1".to_string(),
                call_id: "cal_1".to_string(),
            })
        );
        // The question tool never asks for permission.
        assert!(ask.requests().is_empty());
    }

    #[tokio::test]
    async fn single_question_title_and_no_tool_without_call_id() {
        let temp = TempDir::new("question-single");
        let fake = Arc::new(FakeQuestion::default());
        fake.enqueue(vec![vec!["a".to_string(), "b".to_string()]]);
        let def = tool(Arc::clone(&fake) as Arc<dyn Question>, temp.path());
        let ask = RecordingAsk::new();
        let inst = instance(temp.path());
        let extra = Extra::default();
        let ctx = ctx(&ask, &inst, &extra, None);

        let result = (def.execute)(
            json!({ "questions": [
                { "question": "Pick two?", "header": "Pick",
                  "options": [{ "label": "a", "description": "A" }] },
            ] }),
            ctx,
        )
        .await
        .unwrap();

        assert_eq!(result.title, "Asked 1 question");
        assert_eq!(
            result.output,
            "User has answered your questions: \"Pick two?\"=\"a, b\". You can now continue with the user's answers in mind."
        );
        assert_eq!(fake.asks.lock().unwrap()[0].tool, None);
    }

    #[tokio::test]
    async fn rejection_carries_the_dismissed_message() {
        let temp = TempDir::new("question-reject");
        let fake = Arc::new(FakeQuestion {
            reject_all: true,
            ..FakeQuestion::default()
        });
        let def = tool(Arc::clone(&fake) as Arc<dyn Question>, temp.path());
        let ask = RecordingAsk::new();
        let inst = instance(temp.path());
        let extra = Extra::default();
        let first = ctx(&ask, &inst, &extra, Some("cal_1"));

        let err = (def.execute)(
            json!({ "questions": [
                { "question": "Proceed?", "header": "Confirm",
                  "options": [{ "label": "yes", "description": "Proceed" }] },
            ] }),
            first,
        )
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), "The user dismissed this question");

        // An exhausted queue rejects too.
        let fake = Arc::new(FakeQuestion::default());
        let def = tool(Arc::clone(&fake) as Arc<dyn Question>, temp.path());
        let err = (def.execute)(
            json!({ "questions": [
                { "question": "Proceed?", "header": "Confirm",
                  "options": [{ "label": "yes", "description": "Proceed" }] },
            ] }),
            ctx(&ask, &inst, &extra, None),
        )
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), REJECTED_MESSAGE);
    }

    #[test]
    fn rejected_message_is_byte_exact() {
        assert_eq!(rejected().to_string(), "The user dismissed this question");
        assert_eq!(REJECTED_MESSAGE, "The user dismissed this question");
    }
}

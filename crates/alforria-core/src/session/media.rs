//! Media a model cannot read (an alforria extension).
//!
//! An image sent to a model declared without image input is not refused:
//! OpenAI-compatible servers drop it silently, and the model answers that
//! nothing was attached. So the request is checked before it goes out: an
//! image the target model cannot read is replaced, in the request only, by a
//! line saying so. The stored message keeps its image, and a later turn on
//! a model that can read it sends it as before.

use alforria_llm::schema::messages::{ContentPart, Message, ToolResultValue};
use alforria_schema::llm::ToolContent;
use alforria_schema::session_v1::{V1Message, V1Part};
use serde_json::json;

use crate::event::bus::{EventBus, PublishOptions};
use crate::event::definition::Definition;
use crate::session::error::SessionError;
use crate::session::ids::PartId;
use crate::session::message::{message_id, WithParts};
use crate::session::store::SessionStore;

/// Published when a request went out without media its model cannot read:
/// `{ sessionID, modelID, providerID, omitted: [{ mime, filename? }] }`.
pub const MEDIA_OMITTED: Definition = Definition::ephemeral("session.media.omitted");

/// The kinds of media a model takes as input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaSupport {
    pub image: bool,
}

impl Default for MediaSupport {
    /// A model about which nothing is known is sent everything, as before.
    fn default() -> Self {
        MediaSupport { image: true }
    }
}

impl MediaSupport {
    fn reads(&self, mime: &str) -> bool {
        self.image || !mime.starts_with("image/")
    }
}

/// One piece of media left out of a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OmittedMedia {
    pub mime: String,
    pub filename: Option<String>,
}

/// What stands in a request for media its model cannot read.
///
/// This is the one place that decides it. A product with a second,
/// vision-capable model can describe the image here instead (given the
/// media's bytes and a delegate to call); until one does, the model is told
/// plainly that something was there.
pub fn unreadable_media_text(media: &OmittedMedia) -> String {
    if media.mime.starts_with("image/") {
        "[image omitted: the current model cannot read images]".to_string()
    } else {
        format!(
            "[{} omitted: the current model cannot read it]",
            media.filename.as_deref().unwrap_or("attachment")
        )
    }
}

/// Replace, in a request about to be sent, every piece of media `support`
/// says the model cannot read. Returns what was left out.
pub fn omit_unreadable_media(
    messages: &mut [Message],
    support: &MediaSupport,
) -> Vec<OmittedMedia> {
    let mut omitted = Vec::new();
    for message in messages {
        for part in &mut message.content {
            match part {
                ContentPart::Media {
                    media_type,
                    filename,
                    ..
                } if !support.reads(media_type) => {
                    let media = OmittedMedia {
                        mime: media_type.clone(),
                        filename: filename.clone(),
                    };
                    *part = Message::text(unreadable_media_text(&media));
                    omitted.push(media);
                }
                ContentPart::ToolResult {
                    result: ToolResultValue::Content { value },
                    ..
                } => {
                    for item in value {
                        let ToolContent::File { mime, name, .. } = item else {
                            continue;
                        };
                        if support.reads(mime) {
                            continue;
                        }
                        let media = OmittedMedia {
                            mime: mime.clone(),
                            filename: name.clone(),
                        };
                        *item = ToolContent::Text {
                            text: unreadable_media_text(&media),
                        };
                        omitted.push(media);
                    }
                }
                _ => {}
            }
        }
    }
    omitted
}

/// The request media was left out of.
#[derive(Clone, Copy)]
pub struct MediaRequest<'a> {
    pub session_id: &'a str,
    pub provider_id: &'a str,
    pub model_id: &'a str,
    /// The stored messages the request was rendered from.
    pub history: &'a [WithParts],
    pub support: &'a MediaSupport,
}

/// The note shown to the person (never sent to the model) on a message
/// whose images the model could not be given.
fn note_text(model: &str, count: usize) -> String {
    if count == 1 {
        format!("Image not sent: {model} cannot read images.")
    } else {
        format!("{count} images not sent: {model} cannot read images.")
    }
}

fn is_note(part: &V1Part) -> bool {
    matches!(
        part,
        V1Part::Text { metadata: Some(metadata), .. } if metadata.contains_key("mediaOmitted")
    )
}

/// Tell the client that media was left out: an event, and on each user
/// message that carried an image the model cannot read, one note for the
/// person. The note is an `ignored` text part, which clients show and the
/// model is never sent; it is written once, however many requests the
/// message goes out in.
pub fn surface_omitted_media(
    sessions: &SessionStore,
    events: &EventBus,
    request: &MediaRequest<'_>,
    omitted: &[OmittedMedia],
) -> Result<(), SessionError> {
    if omitted.is_empty() {
        return Ok(());
    }
    let MediaRequest {
        session_id,
        provider_id,
        model_id,
        history,
        support,
    } = *request;
    for message in history {
        if !matches!(message.info, V1Message::User { .. }) || message.parts.iter().any(is_note) {
            continue;
        }
        let images = message
            .parts
            .iter()
            .filter(|part| matches!(part, V1Part::File { mime, .. } if !support.reads(mime)))
            .count();
        if images == 0 {
            continue;
        }
        sessions.update_part(&V1Part::Text {
            id: PartId::ascending(None)?,
            session_id: session_id.to_string(),
            message_id: message_id(&message.info).to_string(),
            text: note_text(model_id, images),
            synthetic: Some(true),
            ignored: Some(true),
            time: None,
            metadata: json!({ "mediaOmitted": { "providerID": provider_id, "modelID": model_id } })
                .as_object()
                .cloned(),
        })?;
    }
    events.publish(
        &MEDIA_OMITTED,
        json!({
            "sessionID": session_id,
            "providerID": provider_id,
            "modelID": model_id,
            "omitted": omitted
                .iter()
                .map(|media| match &media.filename {
                    Some(filename) => json!({ "mime": media.mime, "filename": filename }),
                    None => json!({ "mime": media.mime }),
                })
                .collect::<Vec<_>>(),
        }),
        PublishOptions::default(),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image() -> ContentPart {
        ContentPart::Media {
            media_type: "image/png".to_string(),
            data: "aGVsbG8=".to_string(),
            filename: Some("shot.png".to_string()),
            metadata: None,
        }
    }

    #[test]
    fn an_image_a_model_cannot_read_becomes_a_line_saying_so() {
        let request = || {
            vec![
                Message {
                    id: None,
                    role: alforria_llm::schema::ids::MessageRole::User,
                    content: vec![Message::text("what is in this picture?"), image()],
                    metadata: None,
                    native: None,
                },
                Message {
                    id: None,
                    role: alforria_llm::schema::ids::MessageRole::Tool,
                    content: vec![ContentPart::ToolResult {
                        id: "call_1".to_string(),
                        name: "read".to_string(),
                        result: ToolResultValue::Content {
                            value: vec![
                                ToolContent::Text {
                                    text: "Image read successfully".to_string(),
                                },
                                ToolContent::File {
                                    uri: "aGVsbG8=".to_string(),
                                    mime: "image/jpeg".to_string(),
                                    name: None,
                                },
                                ToolContent::File {
                                    uri: "JVBERi0=".to_string(),
                                    mime: "application/pdf".to_string(),
                                    name: Some("spec.pdf".to_string()),
                                },
                            ],
                        },
                        provider_executed: None,
                        cache: None,
                        metadata: None,
                        provider_metadata: None,
                    }],
                    metadata: None,
                    native: None,
                },
            ]
        };

        // A model that reads images is sent the request untouched.
        let mut sent = request();
        assert!(omit_unreadable_media(&mut sent, &MediaSupport::default()).is_empty());
        assert_eq!(sent, request());

        let mut sent = request();
        let omitted = omit_unreadable_media(&mut sent, &MediaSupport { image: false });
        assert_eq!(
            omitted,
            vec![
                OmittedMedia {
                    mime: "image/png".to_string(),
                    filename: Some("shot.png".to_string()),
                },
                OmittedMedia {
                    mime: "image/jpeg".to_string(),
                    filename: None,
                },
            ]
        );
        let text = serde_json::to_string(&sent).unwrap();
        assert!(!text.contains("aGVsbG8="), "{text}");
        assert_eq!(
            text.matches("[image omitted: the current model cannot read images]")
                .count(),
            2,
            "{text}"
        );
        // Only images are held back.
        assert!(text.contains("JVBERi0="), "{text}");
        assert!(text.contains("what is in this picture?"), "{text}");
    }

    /// A turn with an image, on a model that reads images or does not.
    async fn turn_with_an_image(
        name: &str,
        reads_images: bool,
    ) -> (crate::session::test_support::Engine, String) {
        use crate::session::prompt_input::{PromptInput, PromptPartInput};
        use crate::session::test_support::{
            create_engine_session, engine_with_model, test_model, text_stream,
        };
        let mut model = test_model();
        model.media = MediaSupport {
            image: reads_images,
        };
        let e = engine_with_model(
            name,
            vec![text_stream("first"), text_stream("second")],
            model,
        );
        let session = create_engine_session(&e);
        let prompt = |text: &str, image: bool| {
            let mut parts = vec![PromptPartInput::Text {
                id: None,
                text: text.to_string(),
                synthetic: None,
                ignored: None,
                time: None,
                metadata: None,
            }];
            if image {
                parts.push(PromptPartInput::File {
                    id: None,
                    mime: "image/png".to_string(),
                    filename: Some("shot.png".to_string()),
                    url: "data:image/png;base64,aGVsbG8=".to_string(),
                    source: None,
                });
            }
            PromptInput {
                session_id: session.id.clone(),
                message_id: None,
                model: Some(crate::session::prompt_input::ModelRef {
                    provider_id: "anthropic".to_string(),
                    model_id: "claude".to_string(),
                }),
                agent: Some("build".to_string()),
                no_reply: None,
                tools: None,
                format: None,
                system: None,
                variant: None,
                parts,
                caller: crate::session::caller::Caller::Operator,
            }
        };
        e.prompt
            .prompt(prompt("what is in this picture?", true))
            .await
            .unwrap();
        // A second turn sends the first message again.
        e.prompt.prompt(prompt("and now?", false)).await.unwrap();
        (e, session.id)
    }

    #[tokio::test]
    async fn a_model_without_image_input_is_told_an_image_was_there() {
        let (e, session) = turn_with_an_image("media-omitted", false).await;
        let mut events = e.services.events.subscribe("session.media.omitted");

        for request in [e.llm.input(0), e.llm.input(1)] {
            let sent = serde_json::to_string(&request.messages).unwrap();
            assert!(!sent.contains("aGVsbG8="), "{sent}");
            assert!(
                sent.contains("[image omitted: the current model cannot read images]"),
                "{sent}"
            );
            assert!(sent.contains("what is in this picture?"), "{sent}");
            // The note is for the person, not the model.
            assert!(!sent.contains("Image not sent"), "{sent}");
        }

        // The stored message keeps its image, and gains one note however
        // many requests it went out in.
        let history = e.services.sessions.messages(&session, None).unwrap();
        let user = &history[0];
        assert!(user.parts.iter().any(|part| matches!(
            part,
            V1Part::File { url, .. } if url == "data:image/png;base64,aGVsbG8="
        )));
        let notes: Vec<&V1Part> = user.parts.iter().filter(|part| is_note(part)).collect();
        assert_eq!(notes.len(), 1, "{:?}", user.parts);
        let V1Part::Text {
            text,
            ignored,
            synthetic,
            ..
        } = notes[0]
        else {
            panic!("a text part");
        };
        assert_eq!(text, "Image not sent: claude cannot read images.");
        assert_eq!((*ignored, *synthetic), (Some(true), Some(true)));

        // Clients are told as it happens.
        e.prompt
            .prompt(crate::session::e2e::prompt_input_for(
                "msg_zzzzzzzzzzzzzzzzzzzzzzzzzz",
                &session,
                "again",
            ))
            .await
            .unwrap();
        let event = events.try_recv().expect("an event");
        assert_eq!(event.data["sessionID"], json!(session));
        assert_eq!(event.data["modelID"], "claude");
        assert_eq!(
            event.data["omitted"],
            json!([{ "mime": "image/png", "filename": "shot.png" }])
        );
    }

    #[tokio::test]
    async fn a_model_with_image_input_is_sent_the_image() {
        let (e, session) = turn_with_an_image("media-sent", true).await;
        let sent = serde_json::to_string(&e.llm.input(0).messages).unwrap();
        assert!(sent.contains("aGVsbG8="), "{sent}");
        assert!(!sent.contains("image omitted"), "{sent}");
        let history = e.services.sessions.messages(&session, None).unwrap();
        assert!(!history[0].parts.iter().any(is_note));
    }
}

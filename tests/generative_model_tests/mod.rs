use std::sync::Arc;

use futures::StreamExt;
use myco::generative_model::{Content, GenerateOutput, GenerativeModel, Message, TurnEndReason};

mod anthropic;
mod openai_responses;

/// Multi-turn messaging smoke test against any dyn GenerativeModel.
pub async fn test_generative_model_messaging(model: Arc<dyn GenerativeModel>) {
    let mut history: Vec<Message> = Vec::new();

    let question_answer_pairs = [
        ("What is the capital of France?", "paris"),
        ("How about the United States?", "washington"),
    ];

    for (user_message_str, response_passphrase) in question_answer_pairs {
        history.push(Message::UserMessage {
            content: vec![Content::Text {
                text: user_message_str.to_string(),
            }],
        });

        let stream = model.generate(&history);
        let output = GenerateOutput::from_stream(stream).await.unwrap();

        assert!(output.tool_uses.is_empty());
        // Models may return Thinking + Text (or multiple text parts); assert on text only.
        let text = output
            .content
            .iter()
            .filter_map(|c| match c {
                Content::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !text.is_empty(),
            "expected at least one text content block, got {:?}",
            output.content
        );
        assert!(
            text.to_lowercase().contains(response_passphrase),
            "expected {:?} to contain {:?}",
            text,
            response_passphrase
        );

        history.push(Message::AssistantMessage {
            content: output.content,
            tool_uses: output.tool_uses,
            turn_end_reason: Some(output.turn_end_reason),
        });
    }
}

/// Collect a full stream without going through GenerateOutput (sanity).
#[allow(dead_code)]
pub async fn drain_turn_end_reason(
    model: &dyn GenerativeModel,
    history: &[Message],
) -> TurnEndReason {
    let mut stream = model.generate(history);
    let mut reason = None;
    while let Some(part) = stream.next().await {
        if let Ok(myco::generative_model::MessagePart::TurnEndReason(r)) = part {
            reason = Some(r);
        }
    }
    reason.expect("stream should end with TurnEndReason")
}

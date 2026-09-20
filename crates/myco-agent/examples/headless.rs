//! An offline model/tool round using only the public library interfaces.

use std::sync::Arc;

use futures::stream;
use myco_agent::{Agent, Async, CancelToken, NullEventSink, ToolExecutor};
use myco_model::{
    AsyncStream, Content, ContentDelta, ContentStart, GenerationEvent, GenerativeModel, Message,
    MessagePart, ToolResult, ToolSpec, ToolUse, ToolUseDelta, ToolUseStart, TurnEndReason,
};
use serde_json::json;

// ANCHOR: tools
struct EchoTools;

impl ToolExecutor for EchoTools {
    fn tool_specs(&self) -> Vec<ToolSpec> {
        vec![ToolSpec {
            name: "echo".into(),
            description: "Return the supplied text.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"],
                "additionalProperties": false
            }),
        }]
    }

    fn dispatch(self: Arc<Self>, tool: ToolUse, cancel: CancelToken) -> Async<ToolResult> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return ToolResult::err("cancelled");
            }
            if tool.name != "echo" {
                return ToolResult::err("unknown tool");
            }
            match tool.input.as_object() {
                Some(input) if input.len() == 1 => match input.get("text").and_then(|v| v.as_str())
                {
                    Some(text) => ToolResult::text(text),
                    None => ToolResult::err("echo requires a string field: text"),
                },
                _ => ToolResult::err("echo expects an object containing only text"),
            }
        })
    }
}
// ANCHOR_END: tools

struct DemoModel;

impl GenerativeModel for DemoModel {
    fn generate(&self, input: &[Message]) -> AsyncStream<GenerationEvent> {
        let mut parts = vec![MessagePart::MessageStart];
        if let Some(Message::ToolResults { tool_use_results }) = input.last() {
            let text = tool_use_results
                .iter()
                .flat_map(|result| &result.content)
                .filter_map(|content| match content {
                    Content::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            parts.extend([
                MessagePart::ContentStart(ContentStart::Text { index: 0 }),
                MessagePart::ContentDelta(ContentDelta::Text {
                    index: 0,
                    delta: text,
                }),
                MessagePart::TurnEndReason(TurnEndReason::EndTurn),
            ]);
        } else {
            parts.extend([
                MessagePart::ToolUseStart(ToolUseStart {
                    index: 0,
                    name: "echo".into(),
                }),
                MessagePart::ToolUseDelta(ToolUseDelta {
                    index: 0,
                    input_json_delta: json!({"text": "Hello from a headless agent."}).to_string(),
                }),
                MessagePart::TurnEndReason(TurnEndReason::ToolUse),
            ]);
        }
        Box::pin(stream::iter(parts.into_iter().map(GenerationEvent::Part)))
    }
}

// ANCHOR: run
#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let tools = Arc::new(EchoTools);
    let mut agent = Agent::new(Arc::new(DemoModel), tools, Arc::new(NullEventSink));
    agent.append_input(Message::UserMessage {
        content: vec![Content::Text {
            text: "Use echo to say hello.".into(),
        }],
    });

    let answer = agent.run(CancelToken::new()).await?;
    for content in answer {
        if let Content::Text { text } = content {
            println!("{text}");
        }
    }
    // Persist agent.history() here if your application needs durable history.
    Ok(())
}
// ANCHOR_END: run

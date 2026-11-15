//! Scripted generation streams and in-memory tool execution for agent tests.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use futures::stream;

use myco_model::AsyncStream;
use myco_model::{
    Content, GenerateError, GenerateOutput, GenerationEvent, GenerationFailure, GenerativeModel,
    Message, ToolResult, ToolUse, TurnEndReason,
};

use crate::{Agent, AgentInteractionError, Async, CancelToken, ToolExecutor};

// ---------------------------------------------------------------------------
// ScriptedModel
// ---------------------------------------------------------------------------

/// Scripted model: each `generate` call consumes the next pre-baked
/// [`GenerateOutput`] (FIFO) and replays it as a stream of [`MessagePart`]s —
/// same shape the agent sees from a real provider. Once the scripts drain,
/// every call yields the [`Self::then_fail`] error, or panics if none is set.
pub(crate) struct ScriptedModel {
    scripts: Mutex<VecDeque<GenerateOutput>>,
    fail: Mutex<Option<GenerateError>>,
}

impl ScriptedModel {
    pub(crate) fn new(scripts: Vec<GenerateOutput>) -> Arc<Self> {
        Arc::new(Self {
            scripts: Mutex::new(scripts.into()),
            fail: Mutex::new(None),
        })
    }

    /// Fail every call after the scripts drain (`new(vec![]).then_fail(err)`
    /// is a model that always fails).
    pub(crate) fn then_fail(self: Arc<Self>, err: GenerateError) -> Arc<Self> {
        *self.fail.lock().expect("fail lock") = Some(err);
        self
    }

    /// How many scripted turns remain (for assertions).
    pub(crate) fn remaining(&self) -> usize {
        self.scripts.lock().expect("scripts lock").len()
    }
}

impl GenerativeModel for ScriptedModel {
    fn generate(&self, _input: &[Message]) -> AsyncStream<GenerationEvent> {
        let Some(output) = self.scripts.lock().expect("scripts lock").pop_front() else {
            let err = self.fail.lock().expect("fail lock").clone();
            let err = err.expect("scripted model ran out of outputs");
            return Box::pin(stream::once(async move {
                GenerationEvent::Failure(GenerationFailure::terminal(err))
            }));
        };

        Box::pin(stream::iter(
            output.into_parts().into_iter().map(GenerationEvent::Part),
        ))
    }
}

// ---------------------------------------------------------------------------
// Message / conversation fixtures
// ---------------------------------------------------------------------------

pub(crate) fn user(text: &str) -> Message {
    Message::UserMessage {
        content: vec![Content::Text { text: text.into() }],
    }
}

pub(crate) fn assistant(text: &str) -> Message {
    Message::AssistantMessage {
        content: vec![Content::Text { text: text.into() }],
        tool_uses: vec![],
        turn_end_reason: Some(TurnEndReason::EndTurn),
    }
}

pub(crate) fn assistant_tool(text: Option<&str>, name: &str, input: serde_json::Value) -> Message {
    Message::AssistantMessage {
        content: text
            .map(|t| vec![Content::Text { text: t.into() }])
            .unwrap_or_default(),
        tool_uses: vec![ToolUse {
            name: name.into(),
            input,
            thought_signature: None,
        }],
        turn_end_reason: Some(TurnEndReason::ToolUse),
    }
}

pub(crate) fn tool_results(results: &[&str]) -> Message {
    Message::ToolResults {
        tool_use_results: results.iter().map(|text| ToolResult::text(*text)).collect(),
    }
}

// ---------------------------------------------------------------------------
// ToolResult text extraction
// ---------------------------------------------------------------------------

pub(crate) fn text_parts(result: &ToolResult) -> Vec<String> {
    result
        .content
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

pub(crate) fn result_text(result: &ToolResult) -> String {
    text_parts(result).join("\n")
}

pub(crate) async fn interact(
    agent: &mut Agent,
    input: Vec<Content>,
    cancel: CancelToken,
) -> Result<Vec<Content>, AgentInteractionError> {
    agent.append_input(Message::UserMessage { content: input })?;
    agent.run(cancel).await
}

pub(crate) struct TestTools(Vec<Arc<dyn ToolExecutor>>);

impl TestTools {
    pub(crate) fn new(tools: Vec<Arc<dyn ToolExecutor>>) -> Arc<Self> {
        Arc::new(Self(tools))
    }
}

impl ToolExecutor for TestTools {
    fn tool_specs(&self) -> Vec<myco_model::ToolSpec> {
        self.0.iter().flat_map(|tool| tool.tool_specs()).collect()
    }

    fn dispatch(
        self: Arc<Self>,
        call: ToolUse,
        cancel: CancelToken,
        background: CancelToken,
    ) -> Async<ToolResult> {
        match self
            .0
            .iter()
            .find(|tool| tool.tool_specs().iter().any(|spec| spec.name == call.name))
        {
            Some(tool) => tool.clone().dispatch(call, cancel, background),
            None => {
                Box::pin(async move { ToolResult::err(format!("unknown tool '{}'", call.name)) })
            }
        }
    }
}

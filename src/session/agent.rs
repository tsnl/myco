use std::sync::Arc;

use futures::future;
use uuid::Uuid;

use crate::core::CancelToken;
use crate::generative_model::{
    self, Content, ContentDelta, GenerateError, GenerateOutput, GenerativeModel, Message,
    MessagePart, Recovery, TokenUsage, ToolResult, ToolUse, TurnEndReason, answer_content,
};
use crate::harness::Harness;

//
// Event sink — live observability for agent / tool activity
//

/// Format a UUID as a 32-char lowercase hex string (no hyphens).
pub fn uuid_simple_hex(id: Uuid) -> String {
    id.as_simple().to_string()
}

/// Correlation / nesting context carried on every event.
///
/// Every running agent (root or nested) has a stable [`Self::agent_id`]. Nesting is expressed with
/// `depth` and optional `parent_tool_use_id`, not with separate event types per agent role.
///
/// Tool-use ids remain provider opaque strings (Anthropic `toolu_…`, OpenAI `call_…`); they are
/// not UUIDs and must round-trip unchanged to the model API.
#[derive(Debug, Clone)]
pub struct TraceContext {
    /// Stable id for this agent session (root or subagent).
    pub agent_id: Uuid,
    /// Nesting depth: root agent is 0; each nested agent is parent depth + 1.
    pub depth: usize,
    /// Tool use that is currently in flight / spawned this nested work, if any.
    /// Provider-issued id (string), not a UUID.
    pub parent_tool_use_id: Option<String>,
}

impl Default for TraceContext {
    fn default() -> Self {
        Self {
            agent_id: Uuid::nil(),
            depth: 0,
            parent_tool_use_id: None,
        }
    }
}

impl TraceContext {
    pub fn root() -> Self {
        Self {
            agent_id: Uuid::new_v4(),
            depth: 0,
            parent_tool_use_id: None,
        }
    }

    pub fn child_agent(&self, agent_id: Uuid, parent_tool_use_id: Option<String>) -> Self {
        Self {
            agent_id,
            depth: self.depth + 1,
            parent_tool_use_id,
        }
    }
}

/// Live events emitted by the agent runtime.
///
/// Every variant carries the [`TraceContext`] of the agent that produced it, so
/// a sink can attribute work and filter by nesting depth. Turn-accumulated
/// token usage is *not* an event: it is read from [`Agent::last_usage`] at the
/// prompt, where the CLI needs it.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// Incremental assistant text (for streaming UX).
    TextDelta { text: String, context: TraceContext },
    /// Incremental thinking *summary* text (streamed for UI; also stored in history).
    ThinkingDelta { text: String, context: TraceContext },
    ToolStarted {
        tool_use: ToolUse,
        context: TraceContext,
    },
    TurnFinished {
        reason: TurnEndReason,
        context: TraceContext,
    },
}

/// Consumer of [`AgentEvent`]s (CLI, TUI, metrics, …).
pub trait EventSink: Send + Sync {
    fn emit(&self, event: AgentEvent);
}

/// No-op sink for tests and headless runs.
#[derive(Debug, Default)]
pub struct NullEventSink;

impl EventSink for NullEventSink {
    fn emit(&self, _event: AgentEvent) {}
}

//
// Agent
//

/// Callback invoked at well-formed mid-turn history boundaries (after the user
/// message is pushed and after each ToolResults push) so callers can persist
/// the conversation before the turn completes. Not called between an assistant
/// tool_use message and its results — that prefix is rejected by providers, so
/// it must never be the snapshot a context fork inherits.
pub type HistoryCheckpoint = Box<dyn Fn(&[Message], Option<TokenUsage>) + Send + Sync>;

/// How long a cancelled tool dispatch may keep running to do its own
/// cleanup (process-group kill, buffer drain) before the agent abandons it
/// and records a synthetic cancelled result.
const CANCEL_TOOL_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

pub struct Agent {
    model: Arc<dyn GenerativeModel>,
    harness: Arc<Harness>,
    sink: Arc<dyn EventSink>,
    context: TraceContext,
    history: Vec<Message>,
    /// Last turn's usage: input = final request's prompt (context estimate for
    /// the next USER header), output = summed across that turn's generate calls.
    last_usage: Option<TokenUsage>,
    /// Context window for the active model (tokens).
    context_window_tokens: u64,
    checkpoint: Option<HistoryCheckpoint>,
}

impl Agent {
    pub fn new(
        model: Arc<dyn GenerativeModel>,
        harness: Arc<Harness>,
        sink: Arc<dyn EventSink>,
    ) -> Self {
        Self::with_context(model, harness, sink, TraceContext::root())
    }

    pub fn with_context(
        model: Arc<dyn GenerativeModel>,
        harness: Arc<Harness>,
        sink: Arc<dyn EventSink>,
        context: TraceContext,
    ) -> Self {
        Self {
            model,
            harness,
            sink,
            context,
            history: Vec::new(),
            last_usage: None,
            context_window_tokens: 200_000,
            checkpoint: None,
        }
    }

    /// Install the mid-turn history checkpoint (see [`HistoryCheckpoint`]).
    pub fn set_checkpoint(&mut self, checkpoint: HistoryCheckpoint) {
        self.checkpoint = Some(checkpoint);
    }

    fn emit_checkpoint(&self) {
        if let Some(checkpoint) = &self.checkpoint {
            checkpoint(&self.history, self.last_usage);
        }
    }

    pub fn history(&self) -> &[Message] {
        &self.history
    }

    /// Replace the conversation history (e.g. when resuming a saved session).
    pub fn set_history(&mut self, history: Vec<Message>) {
        self.history = history;
    }

    /// Swap the generative model (e.g. mid-session `/effort` rebuild). History is kept.
    pub fn set_model(&mut self, model: Arc<dyn GenerativeModel>) {
        self.model = model;
    }

    /// Drop the last user turn — that message and everything the agent produced
    /// after it — and return the removed user content.
    ///
    /// The recovery for [`Recovery::OmitLastMessage`]: a request the provider
    /// rejects for its *size* is rejected again on every later turn, because
    /// every later turn resends it. Truncating at the last `UserMessage` leaves
    /// a well-formed prefix — the boundary is exactly where the previous turn
    /// ended — so the session continues instead of failing forever.
    ///
    /// `None` when there is no user message to remove; history is untouched.
    pub fn rewind_last_user_turn(&mut self) -> Option<Vec<Content>> {
        let at = self
            .history
            .iter()
            .rposition(|m| matches!(m, Message::UserMessage { .. }))?;
        let mut dropped = self.history.split_off(at);
        // Usage described the request that just failed; it no longer describes
        // this history. Callers re-establish it on the next successful turn.
        self.last_usage = None;
        self.emit_checkpoint();
        match dropped.remove(0) {
            Message::UserMessage { content } => Some(content),
            other => unreachable!("rposition matched a user message, got {other:?}"),
        }
    }

    /// Set the context window used for the USER `N/M` token header.
    pub fn set_context_window_tokens(&mut self, tokens: u64) {
        self.context_window_tokens = tokens.max(1);
    }

    pub fn context_window_tokens(&self) -> u64 {
        self.context_window_tokens
    }

    /// Last observed prompt/context token usage (from the provider), if any.
    pub fn last_usage(&self) -> Option<TokenUsage> {
        self.last_usage
    }

    /// Seed last-usage when resuming a saved session (`None` if never tracked).
    pub fn set_last_usage(&mut self, usage: Option<TokenUsage>) {
        self.last_usage = usage;
    }

    pub fn context(&self) -> &TraceContext {
        &self.context
    }

    /// Run one user turn until the model ends the turn or [`cancel`] fires.
    ///
    /// Pass [`CancelToken::new`] when cancellation is not needed (tests, scripts).
    /// The CLI cancels the token on Ctrl-C while a turn is in flight.
    pub async fn interact(
        &mut self,
        user_input: Vec<Content>,
        cancel: CancelToken,
    ) -> Result<Vec<Content>, AgentInteractionError> {
        self.history.push(Message::UserMessage {
            content: user_input,
        });
        self.emit_checkpoint();

        // Output tokens accumulate across this turn's generate calls (one per
        // tool round-trip); each new report's input side already covers the
        // whole prompt, so it replaces rather than adds.
        let mut turn_output: u64 = 0;

        loop {
            if cancel.is_cancelled() {
                return self.finish_cancelled();
            }

            let stream = self.model.generate(&self.history);
            let sink = self.sink.clone();
            let context = self.context.clone();
            let output = match accumulate_generate(stream, sink, context, cancel.clone()).await {
                Ok(output) => output,
                Err(GenerateOrCancel::Cancelled) => return self.finish_cancelled(),
                // finish_generate_error emits TurnFinished so live ASSISTANT closes
                // before the CLI opens an ERROR section.
                Err(GenerateOrCancel::Generate(e)) => return self.finish_generate_error(e),
            };

            if let Some(usage) = output.usage {
                turn_output += usage.output_tokens;
                let usage = TokenUsage {
                    output_tokens: turn_output,
                    ..usage
                };
                self.last_usage = Some(usage);
            }

            let reason = output.turn_end_reason.clone();

            // A tool_use stop with zero accumulated tool calls is malformed
            // (e.g. a content block the accumulator ignored). Retrying with
            // unchanged history would loop generate forever, and pushing an
            // empty ToolResults message is rejected by the API — fail loud.
            if matches!(reason, TurnEndReason::ToolUse) && output.tool_uses.is_empty() {
                self.history.push(Message::AssistantMessage {
                    content: output.content,
                    tool_uses: vec![],
                    turn_end_reason: Some(TurnEndReason::ToolUse),
                });
                return self.finish_generate_error(GenerateError::MalformedResponseError(
                    "turn ended in tool_use but streamed zero tool uses".into(),
                ));
            }

            // Return answer content only; history keeps thinking for resume/UI.
            // Backends strip thinking when composing the next request.
            let content = answer_content(&output.content);
            let tool_uses = output.tool_uses;
            // Persist full content (including thinking summaries) for session
            // resume/UI. Backends strip thinking when composing the next request.
            self.history.push(Message::AssistantMessage {
                content: output.content,
                tool_uses: tool_uses.clone(),
                turn_end_reason: Some(reason.clone()),
            });

            // Tool calls are answered whenever the turn carries them — the stop
            // reason does not decide this. `max_tokens` truncates a turn
            // mid-call, so the block arrives under a non-`tool_use` stop; a
            // tool_use nothing responds to makes the whole history unsendable
            // (every later request resends it), which strands the session on the
            // provider's "tool_use without tool_result" error.
            if !tool_uses.is_empty() {
                // Dispatch every tool use in this turn concurrently. join_all preserves
                // input order so tool_results[i] matches tool_uses[i]; events may
                // interleave freely while tools run. Each tool races against cancel so
                // unfinished work returns a synthetic cancelled ToolResult.
                let tool_use_results = future::join_all(
                    tool_uses
                        .into_iter()
                        .map(|tool_use| self.dispatch_tool_use(tool_use, cancel.clone())),
                )
                .await;

                self.history.push(Message::ToolResults { tool_use_results });
                self.emit_checkpoint();

                // If cancel fired during tools, do not start another generate — the
                // transcript already has matching tool results for every tool_use.
                if cancel.is_cancelled() {
                    return self.finish_cancelled();
                }
            }

            // `tool_use` is the only stop that continues the turn; everything
            // else hands control back with whatever the model managed to say.
            if !matches!(reason, TurnEndReason::ToolUse) {
                self.sink.emit(AgentEvent::TurnFinished {
                    reason,
                    context: self.context.clone(),
                });
                return Ok(content);
            }
        }
    }

    fn finish_cancelled(&self) -> Result<Vec<Content>, AgentInteractionError> {
        self.sink.emit(AgentEvent::TurnFinished {
            reason: TurnEndReason::Other("cancelled".into()),
            context: self.context.clone(),
        });
        Err(AgentInteractionError::Cancelled)
    }

    /// Errors end the turn too: sinks key section/state resets off
    /// `TurnFinished`, so skipping it on error leaves the next turn's output
    /// rendering glued to this one's (and an open `Thinking:` line dangling).
    fn finish_generate_error(
        &self,
        error: GenerateError,
    ) -> Result<Vec<Content>, AgentInteractionError> {
        self.sink.emit(AgentEvent::TurnFinished {
            reason: TurnEndReason::Other("generate_error".into()),
            context: self.context.clone(),
        });
        Err(AgentInteractionError::GenerateError(error))
    }

    async fn dispatch_tool_use(&self, tool_use: ToolUse, cancel: CancelToken) -> ToolResult {
        self.sink.emit(AgentEvent::ToolStarted {
            tool_use: tool_use.clone(),
            context: self.context.clone(),
        });

        // Tools run in the same agent session; only parent_tool_use_id updates for correlation.
        let dispatch_context = TraceContext {
            agent_id: self.context.agent_id,
            depth: self.context.depth,
            parent_tool_use_id: Some(tool_use.id.clone()),
        };

        let work = self.harness.clone().dispatch_tool_use(
            tool_use.clone(),
            dispatch_context,
            cancel.clone(),
        );

        // Race cancel vs tool — but on cancel, give the dispatch a short grace
        // window instead of dropping it immediately. Cancel-aware tools use it
        // to run their own cleanup and return an honest partial result: bash
        // kills the exec's whole process group (kill_on_drop alone SIGKILLs
        // only the leader, orphaning grandchildren), drains its capture tasks,
        // and a mid-write bash session gets its taken ChildStdin back instead
        // of having it dropped (which would close the session's stdin for
        // good). Tools that ignore cancel are abandoned when the grace
        // expires; for subprocess hosts that only abandons this waiter —
        // the pipe demuxes by correlation id, so siblings are unaffected.
        let mut work = std::pin::pin!(work);
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                match tokio::time::timeout(CANCEL_TOOL_GRACE, &mut work).await {
                    Ok(result) => result,
                    Err(_) => ToolResult::err("cancelled").with_id(tool_use.id.clone()),
                }
            }
            result = &mut work => result,
        };

        result
    }
}

enum GenerateOrCancel {
    Cancelled,
    Generate(GenerateError),
}

/// Drain a model stream, forwarding text/thinking deltas, until completion or cancel.
async fn accumulate_generate(
    stream: impl futures::Stream<Item = Result<MessagePart, GenerateError>> + Unpin,
    sink: Arc<dyn EventSink>,
    context: TraceContext,
    cancel: CancelToken,
) -> Result<GenerateOutput, GenerateOrCancel> {
    // Race the full accumulator against cancel. Dropping the stream aborts the
    // underlying HTTP body when the provider future is cancelled.
    let accumulate = GenerateOutput::from_stream_with_hook(stream, |part| match part {
        MessagePart::ContentDelta(ContentDelta::Text { delta, .. }) => {
            sink.emit(AgentEvent::TextDelta {
                text: delta.clone(),
                context: context.clone(),
            });
        }
        MessagePart::ContentDelta(ContentDelta::Thinking { delta, .. }) if !delta.is_empty() => {
            sink.emit(AgentEvent::ThinkingDelta {
                text: delta.clone(),
                context: context.clone(),
            });
        }
        _ => {}
    });

    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(GenerateOrCancel::Cancelled),
        result = accumulate => result.map_err(GenerateOrCancel::Generate),
    }
}

impl Drop for Agent {
    fn drop(&mut self) {
        // Tear down agent-owned harness state (bash sessions, …). Skip the nil id used
        // by some unit tests that never go through TraceContext::root().
        if self.context.agent_id.is_nil() {
            return;
        }
        self.harness.notify_agent_finished(self.context.agent_id);
    }
}

#[derive(thiserror::Error, Debug)]
pub enum AgentInteractionError {
    #[error("Error during generation: {0}")]
    GenerateError(#[from] generative_model::GenerateError),
    /// In-flight turn aborted (e.g. Ctrl-C). History is left well-formed when tools
    /// had already started (synthetic cancelled tool results are recorded).
    #[error("cancelled")]
    Cancelled,
}

impl AgentInteractionError {
    /// Whether the failed turn can be resubmitted as-is, or the last user
    /// message has to be rewound out of history first
    /// ([`Agent::rewind_last_user_turn`]).
    pub fn recovery(&self) -> Recovery {
        match self {
            AgentInteractionError::GenerateError(e) => e.recovery(),
            // History is well-formed after a cancel; the same turn can be re-sent.
            AgentInteractionError::Cancelled => Recovery::Retry,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Async;
    use crate::generative_model::{GenerateError, MessagePart, ToolSpec};
    use crate::tool_services::{HostDispatchContext, ToolService};
    use futures::stream;
    use serde_json::json;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    /// Scripted model: each `generate` call yields the next pre-baked output as a stream.
    /// Construct with scripts in call order (FIFO).
    struct ScriptedModel {
        scripts: Mutex<std::collections::VecDeque<GenerateOutput>>,
    }

    impl ScriptedModel {
        fn new(scripts: Vec<GenerateOutput>) -> Arc<Self> {
            Arc::new(Self {
                scripts: Mutex::new(scripts.into()),
            })
        }
    }

    impl GenerativeModel for ScriptedModel {
        fn generate(
            &self,
            _input: &[Message],
        ) -> crate::core::AsyncStream<Result<MessagePart, GenerateError>> {
            let output = self
                .scripts
                .lock()
                .expect("scripts lock")
                .pop_front()
                .expect("scripted model ran out of outputs");

            let mut parts = vec![MessagePart::MessageStart];
            for (i, c) in output.content.iter().enumerate() {
                match c {
                    Content::Text { text } => {
                        parts.push(MessagePart::ContentStart(
                            generative_model::ContentStart::Text { index: i },
                        ));
                        parts.push(MessagePart::ContentDelta(ContentDelta::Text {
                            index: i,
                            delta: text.clone(),
                        }));
                    }
                    Content::Image { source } => {
                        parts.push(MessagePart::ContentStart(
                            generative_model::ContentStart::Image { index: i },
                        ));
                        parts.push(MessagePart::ContentDelta(ContentDelta::Image {
                            index: i,
                            delta: source.clone(),
                        }));
                    }
                    Content::Thinking {
                        text,
                        signature,
                        redacted,
                    } => {
                        parts.push(MessagePart::ContentStart(
                            generative_model::ContentStart::Thinking {
                                index: i,
                                signature: signature.clone(),
                                redacted: *redacted,
                            },
                        ));
                        if !text.is_empty() && !*redacted {
                            parts.push(MessagePart::ContentDelta(ContentDelta::Thinking {
                                index: i,
                                delta: text.clone(),
                            }));
                        }
                    }
                }
            }
            for (i, tu) in output.tool_uses.iter().enumerate() {
                parts.push(MessagePart::ToolUseStart(generative_model::ToolUseStart {
                    index: i,
                    id: tu.id.clone(),
                    name: tu.name.clone(),
                }));
                parts.push(MessagePart::ToolUseDelta(generative_model::ToolUseDelta {
                    index: i,
                    input_json_delta: tu.input.to_string(),
                }));
            }
            if let Some(usage) = output.usage {
                parts.push(MessagePart::Usage(usage));
            }
            parts.push(MessagePart::TurnEndReason(output.turn_end_reason));

            Box::pin(stream::iter(parts.into_iter().map(Ok)))
        }
    }

    /// Sleeps, records start/end instants, returns the configured label.
    struct SlowService {
        name: String,
        delay: Duration,
        starts: Arc<Mutex<Vec<(String, Instant)>>>,
        ends: Arc<Mutex<Vec<(String, Instant)>>>,
    }

    impl ToolService for SlowService {
        fn tool_specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: self.name.clone(),
                description: format!("slow test tool {}", self.name),
                input_schema: json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false,
                }),
            }]
        }

        fn dispatch_tool_use(
            self: Arc<Self>,
            tool_use: ToolUse,
            _ctx: HostDispatchContext,
        ) -> Async<ToolResult> {
            Box::pin(async move {
                let started = Instant::now();
                self.starts
                    .lock()
                    .unwrap()
                    .push((tool_use.id.clone(), started));
                tokio::time::sleep(self.delay).await;
                let ended = Instant::now();
                self.ends.lock().unwrap().push((tool_use.id.clone(), ended));
                ToolResult::text(format!("done:{}", tool_use.id))
            })
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_tool_uses_overlap_and_preserve_order() {
        let starts = Arc::new(Mutex::new(Vec::new()));
        let ends = Arc::new(Mutex::new(Vec::new()));
        // Long enough that serial execution is unambiguous even under CI load.
        let delay = Duration::from_millis(300);

        // Two distinct tool names so the harness router can host both (same service type).
        let slow_a = Arc::new(SlowService {
            name: "slow_a".into(),
            delay,
            starts: starts.clone(),
            ends: ends.clone(),
        });
        let slow_b = Arc::new(SlowService {
            name: "slow_b".into(),
            delay,
            starts: starts.clone(),
            ends: ends.clone(),
        });

        let harness = Harness::local_with_services(vec![
            slow_a as Arc<dyn ToolService>,
            slow_b as Arc<dyn ToolService>,
        ]);

        let model = ScriptedModel::new(vec![
            GenerateOutput {
                content: vec![],
                tool_uses: vec![
                    ToolUse {
                        id: "call_slow".into(),
                        name: "slow_a".into(),
                        input: json!({}),
                    },
                    ToolUse {
                        id: "call_fast".into(),
                        name: "slow_b".into(),
                        input: json!({}),
                    },
                ],
                turn_end_reason: TurnEndReason::ToolUse,
                usage: None,
            },
            GenerateOutput {
                content: vec![Content::Text {
                    text: "all done".into(),
                }],
                tool_uses: vec![],
                turn_end_reason: TurnEndReason::EndTurn,
                usage: None,
            },
        ]);

        let mut agent = Agent::new(model, harness, Arc::new(NullEventSink));
        let wall_start = Instant::now();
        let reply = agent
            .interact(
                vec![Content::Text {
                    text: "run both".into(),
                }],
                crate::core::CancelToken::new(),
            )
            .await
            .expect("interact");
        let wall = wall_start.elapsed();

        // Reply is the final assistant text.
        assert_eq!(reply.len(), 1);
        match &reply[0] {
            Content::Text { text } => assert_eq!(text, "all done"),
            other => panic!("expected text reply, got {other:?}"),
        }

        // History: user, assistant(tool_use), tool_results, assistant(end).
        let history = agent.history();
        assert_eq!(history.len(), 4);
        match &history[2] {
            Message::ToolResults { tool_use_results } => {
                assert_eq!(tool_use_results.len(), 2);
                // Order matches the original tool_uses list, not completion order.
                assert_eq!(tool_use_results[0].id, "call_slow");
                assert_eq!(tool_use_results[1].id, "call_fast");
                assert!(!tool_use_results[0].is_error);
                assert!(!tool_use_results[1].is_error);
            }
            other => panic!("expected ToolResults, got {other:?}"),
        }

        // Both tools started before either finished → concurrent.
        let starts = starts.lock().unwrap().clone();
        let ends = ends.lock().unwrap().clone();
        assert_eq!(starts.len(), 2);
        assert_eq!(ends.len(), 2);
        let first_end = ends.iter().map(|(_, t)| *t).min().unwrap();
        let last_start = starts.iter().map(|(_, t)| *t).max().unwrap();
        assert!(
            last_start < first_end,
            "expected overlapping execution: last_start={last_start:?} first_end={first_end:?} starts={starts:?} ends={ends:?}"
        );

        // Overlap of starts/ends is the real concurrency signal. Wall clock is
        // only a coarse guard against fully serial execution; allow large slack
        // for CI / parallel suite load (scheduler jitter, other tests).
        assert!(
            wall < delay * 6 + Duration::from_secs(1),
            "expected concurrent wall time ~1 delay, got {wall:?} (delay={delay:?})"
        );
    }

    /// Checkpoints fire after the user push and after ToolResults — never
    /// between an assistant tool_use and its results, a prefix providers
    /// reject and a context fork must never inherit.
    #[tokio::test]
    async fn checkpoint_fires_only_at_well_formed_boundaries() {
        let slow = Arc::new(SlowService {
            name: "slow_a".into(),
            delay: Duration::from_millis(1),
            starts: Arc::new(Mutex::new(Vec::new())),
            ends: Arc::new(Mutex::new(Vec::new())),
        });
        let harness = Harness::local_with_services(vec![slow as Arc<dyn ToolService>]);
        let model = ScriptedModel::new(vec![
            GenerateOutput {
                content: vec![],
                tool_uses: vec![ToolUse {
                    id: "call_1".into(),
                    name: "slow_a".into(),
                    input: json!({}),
                }],
                turn_end_reason: TurnEndReason::ToolUse,
                usage: None,
            },
            GenerateOutput {
                content: vec![Content::Text {
                    text: "done".into(),
                }],
                tool_uses: vec![],
                turn_end_reason: TurnEndReason::EndTurn,
                usage: None,
            },
        ]);
        let mut agent = Agent::new(model, harness, Arc::new(NullEventSink));
        let snapshots: Arc<Mutex<Vec<Vec<Message>>>> = Arc::new(Mutex::new(Vec::new()));
        let record = snapshots.clone();
        agent.set_checkpoint(Box::new(move |history, _usage| {
            record.lock().unwrap().push(history.to_vec());
        }));

        agent
            .interact(
                vec![Content::Text { text: "run".into() }],
                crate::core::CancelToken::new(),
            )
            .await
            .expect("interact");

        let snapshots = snapshots.lock().unwrap();
        assert_eq!(snapshots.len(), 2, "{snapshots:?}");
        assert_eq!(snapshots[0].len(), 1);
        assert!(matches!(snapshots[0][0], Message::UserMessage { .. }));
        assert_eq!(snapshots[1].len(), 3);
        assert!(matches!(snapshots[1][1], Message::AssistantMessage { .. }));
        assert!(matches!(snapshots[1][2], Message::ToolResults { .. }));
    }

    #[tokio::test]
    async fn last_usage_sums_output_across_tool_round_trips() {
        let tool = Arc::new(SlowService {
            name: "fast".into(),
            delay: Duration::ZERO,
            starts: Arc::new(Mutex::new(Vec::new())),
            ends: Arc::new(Mutex::new(Vec::new())),
        });
        let harness = Harness::local_with_services(vec![tool as Arc<dyn ToolService>]);

        let model = ScriptedModel::new(vec![
            GenerateOutput {
                content: vec![],
                tool_uses: vec![ToolUse {
                    id: "call_1".into(),
                    name: "fast".into(),
                    input: json!({}),
                }],
                turn_end_reason: TurnEndReason::ToolUse,
                usage: Some(TokenUsage {
                    input_tokens: 1_000,
                    output_tokens: 500,
                    cached_input_tokens: 800,
                }),
            },
            GenerateOutput {
                content: vec![Content::Text {
                    text: "done".into(),
                }],
                tool_uses: vec![],
                turn_end_reason: TurnEndReason::EndTurn,
                usage: Some(TokenUsage {
                    input_tokens: 1_600,
                    output_tokens: 20,
                    cached_input_tokens: 900,
                }),
            },
        ]);

        let mut agent = Agent::new(model, harness, Arc::new(NullEventSink));
        agent
            .interact(
                vec![Content::Text { text: "go".into() }],
                crate::core::CancelToken::new(),
            )
            .await
            .expect("interact");

        // Input side tracks the latest request (the live context); output sums the turn.
        let usage = agent.last_usage().expect("usage recorded");
        assert_eq!(usage.input_tokens, 1_600);
        assert_eq!(usage.cached_input_tokens, 900);
        assert_eq!(usage.output_tokens, 520);
        assert_eq!(usage.context_tokens(), 1_600);
    }

    /// Slow generate stream: cancel mid-stream must return Cancelled quickly.
    struct SlowStreamModel {
        delay: Duration,
        chunks: usize,
    }

    impl GenerativeModel for SlowStreamModel {
        fn generate(
            &self,
            _input: &[Message],
        ) -> crate::core::AsyncStream<Result<MessagePart, GenerateError>> {
            let delay = self.delay;
            let chunks = self.chunks;
            // State machine: 0 = MessageStart, 1 = ContentStart, 2..chunks+1 = delayed
            // deltas, last = TurnEndReason.
            Box::pin(stream::unfold(0usize, move |step| {
                let delay = delay;
                async move {
                    let last = chunks + 2;
                    if step > last {
                        return None;
                    }
                    let part = if step == 0 {
                        MessagePart::MessageStart
                    } else if step == 1 {
                        MessagePart::ContentStart(generative_model::ContentStart::Text { index: 0 })
                    } else if step <= chunks + 1 {
                        tokio::time::sleep(delay).await;
                        MessagePart::ContentDelta(ContentDelta::Text {
                            index: 0,
                            delta: format!("chunk{}", step - 2),
                        })
                    } else {
                        MessagePart::TurnEndReason(TurnEndReason::EndTurn)
                    };
                    Some((Ok(part), step + 1))
                }
            }))
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_during_generate_returns_cancelled() {
        let harness = Harness::local_with_services(vec![]);
        let model = Arc::new(SlowStreamModel {
            delay: Duration::from_millis(200),
            chunks: 20,
        });
        let mut agent = Agent::new(model, harness, Arc::new(NullEventSink));
        let cancel = crate::core::CancelToken::new();
        let cancel2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel2.cancel();
        });

        let t0 = Instant::now();
        let err = agent
            .interact(vec![Content::Text { text: "go".into() }], cancel)
            .await
            .expect_err("should cancel");
        let elapsed = t0.elapsed();
        assert!(
            matches!(err, AgentInteractionError::Cancelled),
            "got {err:?}"
        );
        assert!(
            // Prompt under light load; allow CI / suite contention headroom.
            elapsed < Duration::from_secs(2),
            "cancel should be prompt, took {elapsed:?}"
        );
        // User message kept; no incomplete assistant pushed.
        assert_eq!(agent.history().len(), 1);
        assert!(matches!(agent.history()[0], Message::UserMessage { .. }));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_during_slow_tool_records_cancelled_result() {
        let starts = Arc::new(Mutex::new(Vec::new()));
        let ends = Arc::new(Mutex::new(Vec::new()));
        let slow = Arc::new(SlowService {
            name: "slow_a".into(),
            // Long enough that a delayed cancel still hits mid-tool under load.
            delay: Duration::from_secs(5),
            starts: starts.clone(),
            ends: ends.clone(),
        });
        let harness = Harness::local_with_services(vec![slow as Arc<dyn ToolService>]);
        let model = ScriptedModel::new(vec![GenerateOutput {
            content: vec![],
            tool_uses: vec![ToolUse {
                id: "call_slow".into(),
                name: "slow_a".into(),
                input: json!({}),
            }],
            turn_end_reason: TurnEndReason::ToolUse,
            usage: None,
        }]);
        // No EndTurn scripted — cancel during tools must stop without another generate.
        let mut agent = Agent::new(model, harness, Arc::new(NullEventSink));
        let cancel = crate::core::CancelToken::new();
        let cancel2 = cancel.clone();
        let starts_bg = starts.clone();
        tokio::spawn(async move {
            // Cancel only after the tool has started (not a fixed sleep race).
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                if !starts_bg.lock().unwrap().is_empty() {
                    break;
                }
                if Instant::now() > deadline {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            cancel2.cancel();
        });

        let t0 = Instant::now();
        let err = agent
            .interact(vec![Content::Text { text: "run".into() }], cancel)
            .await
            .expect_err("should cancel");
        let elapsed = t0.elapsed();
        assert!(matches!(err, AgentInteractionError::Cancelled));
        // A cancel-ignoring tool is abandoned after CANCEL_TOOL_GRACE — the
        // turn must end well before the tool's own 5s delay.
        assert!(
            elapsed < Duration::from_secs(4),
            "should wait only the cancel grace, not the full tool delay, took {elapsed:?}"
        );

        // user + assistant(tool_use) + tool_results (cancelled)
        let history = agent.history();
        assert_eq!(history.len(), 3);
        match &history[2] {
            Message::ToolResults { tool_use_results } => {
                assert_eq!(tool_use_results.len(), 1);
                assert!(tool_use_results[0].is_error);
                let text = tool_use_results[0]
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        Content::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                assert!(text.contains("cancelled"), "{text}");
            }
            other => panic!("expected ToolResults, got {other:?}"),
        }
    }

    /// Model that yields scripted successes then a generate error on a later call.
    struct FailAfterScriptsModel {
        scripts: Mutex<std::collections::VecDeque<GenerateOutput>>,
        fail_message: String,
    }

    impl FailAfterScriptsModel {
        fn new(scripts: Vec<GenerateOutput>, fail_message: impl Into<String>) -> Arc<Self> {
            Arc::new(Self {
                scripts: Mutex::new(scripts.into()),
                fail_message: fail_message.into(),
            })
        }
    }

    impl GenerativeModel for FailAfterScriptsModel {
        fn generate(
            &self,
            _input: &[Message],
        ) -> crate::core::AsyncStream<Result<MessagePart, GenerateError>> {
            let maybe = self.scripts.lock().expect("scripts lock").pop_front();
            match maybe {
                Some(output) => {
                    // Reuse ScriptedModel streaming shape via a one-shot queue.
                    let one = ScriptedModel::new(vec![output]);
                    one.generate(&[])
                }
                None => {
                    let msg = self.fail_message.clone();
                    Box::pin(stream::once(async move {
                        Err(GenerateError::ExecutionError(msg))
                    }))
                }
            }
        }
    }

    #[tokio::test]
    async fn generate_error_after_tool_results_keeps_well_formed_history() {
        let slow = Arc::new(SlowService {
            name: "slow_a".into(),
            delay: Duration::from_millis(1),
            starts: Arc::new(Mutex::new(Vec::new())),
            ends: Arc::new(Mutex::new(Vec::new())),
        });
        let harness = Harness::local_with_services(vec![slow as Arc<dyn ToolService>]);
        let model = FailAfterScriptsModel::new(
            vec![GenerateOutput {
                content: vec![],
                tool_uses: vec![ToolUse {
                    id: "call_1".into(),
                    name: "slow_a".into(),
                    input: json!({}),
                }],
                turn_end_reason: TurnEndReason::ToolUse,
                usage: None,
            }],
            "provider 500 after tools",
        );
        let mut agent = Agent::new(model, harness, Arc::new(NullEventSink));
        let err = agent
            .interact(
                vec![Content::Text {
                    text: "run tool then fail".into(),
                }],
                crate::core::CancelToken::new(),
            )
            .await
            .expect_err("second generate should fail");
        assert!(
            matches!(err, AgentInteractionError::GenerateError(_)),
            "got {err:?}"
        );

        // user + assistant(tool_use) + tool_results — no incomplete assistant.
        let history = agent.history();
        assert_eq!(history.len(), 3, "history={history:?}");
        assert!(matches!(history[0], Message::UserMessage { .. }));
        match &history[1] {
            Message::AssistantMessage {
                tool_uses,
                turn_end_reason,
                ..
            } => {
                assert_eq!(tool_uses.len(), 1);
                assert_eq!(tool_uses[0].id, "call_1");
                assert_eq!(*turn_end_reason, Some(TurnEndReason::ToolUse));
            }
            other => panic!("expected assistant tool_use, got {other:?}"),
        }
        match &history[2] {
            Message::ToolResults { tool_use_results } => {
                assert_eq!(tool_use_results.len(), 1);
                assert_eq!(tool_use_results[0].id, "call_1");
                assert!(!tool_use_results[0].is_error);
            }
            other => panic!("expected ToolResults, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn generate_error_before_assistant_keeps_only_user() {
        let harness = Harness::local_with_services(vec![]);
        let model = FailAfterScriptsModel::new(vec![], "boom on first generate");
        let mut agent = Agent::new(model, harness, Arc::new(NullEventSink));
        let err = agent
            .interact(
                vec![Content::Text { text: "hi".into() }],
                crate::core::CancelToken::new(),
            )
            .await
            .expect_err("generate should fail");
        assert!(matches!(err, AgentInteractionError::GenerateError(_)));
        assert_eq!(agent.history().len(), 1);
        assert!(matches!(agent.history()[0], Message::UserMessage { .. }));
    }

    /// A tool_use stop with zero streamed tool uses must fail loud, not loop
    /// generate forever on unchanged history or push empty ToolResults.
    #[tokio::test]
    async fn tool_use_stop_with_zero_tool_uses_errors_not_loops() {
        let harness = Harness::local_with_services(vec![]);
        let model = ScriptedModel::new(vec![GenerateOutput {
            content: vec![Content::Text { text: "hmm".into() }],
            tool_uses: vec![],
            turn_end_reason: TurnEndReason::ToolUse,
            usage: None,
        }]);
        let mut agent = Agent::new(model, harness, Arc::new(NullEventSink));
        let err = agent
            .interact(
                vec![Content::Text { text: "hi".into() }],
                crate::core::CancelToken::new(),
            )
            .await
            .expect_err("malformed turn should error");
        assert!(matches!(err, AgentInteractionError::GenerateError(_)));
        // History stays well-formed: user + assistant, no ToolResults message.
        assert_eq!(agent.history().len(), 2);
        assert!(matches!(
            agent.history()[1],
            Message::AssistantMessage { .. }
        ));
    }

    /// `max_tokens` can cut a turn off mid-tool-call: the stop reason is not
    /// `tool_use`, but the tool_use block is still in the assistant message
    /// (typically with `input: {}` — the arguments never streamed). Tool calls
    /// are answered on their presence, not on the stop reason, because providers
    /// reject any later request carrying a tool_use with no matching tool_result:
    /// leaving it unanswered fails the very next user message, and every one
    /// after it. Here the truncated call reaches the real bash tool, which
    /// rejects the empty input — an error result, but a well-formed turn.
    #[tokio::test]
    async fn max_tokens_mid_tool_call_answers_the_dangling_tool_use() {
        // The standard local services include the real bash tool.
        let harness = Harness::local_with_services(vec![]);
        let model = ScriptedModel::new(vec![
            GenerateOutput {
                content: vec![Content::Text {
                    text: "let me check".into(),
                }],
                tool_uses: vec![ToolUse {
                    id: "toolu_truncated".into(),
                    name: "bash".into(),
                    input: serde_json::json!({}),
                }],
                turn_end_reason: TurnEndReason::MaxTokens,
                usage: None,
            },
            GenerateOutput {
                content: vec![Content::Text { text: "ok".into() }],
                tool_uses: vec![],
                turn_end_reason: TurnEndReason::EndTurn,
                usage: None,
            },
        ]);
        let mut agent = Agent::new(model, harness, Arc::new(NullEventSink));
        agent
            .interact(
                vec![Content::Text { text: "hi".into() }],
                crate::core::CancelToken::new(),
            )
            .await
            .expect("turn should return the partial answer, not an error");

        // user + assistant(tool_use) + tool_results — the tool_use is answered.
        assert_eq!(agent.history().len(), 3);
        match &agent.history()[2] {
            Message::ToolResults { tool_use_results } => {
                assert_eq!(tool_use_results.len(), 1);
                assert_eq!(tool_use_results[0].id, "toolu_truncated");
                assert!(tool_use_results[0].is_error);
                let text = match &tool_use_results[0].content[0] {
                    Content::Text { text } => text.clone(),
                    other => panic!("expected text, got {other:?}"),
                };
                assert!(text.contains("empty bash input"), "text={text}");
            }
            other => panic!("expected ToolResults, got {other:?}"),
        }

        // And the session continues: the next turn generates against a history
        // the provider accepts, instead of failing on the dangling tool_use.
        let reply = agent
            .interact(
                vec![Content::Text {
                    text: "Continue".into(),
                }],
                crate::core::CancelToken::new(),
            )
            .await
            .expect("follow-up turn should succeed");
        assert!(matches!(&reply[0], Content::Text { text } if text == "ok"));
    }

    /// A turn that ends cleanly with no tool calls gains no ToolResults message.
    #[tokio::test]
    async fn plain_end_turn_pushes_no_tool_results() {
        let harness = Harness::local_with_services(vec![]);
        let model = ScriptedModel::new(vec![GenerateOutput {
            content: vec![Content::Text { text: "hi".into() }],
            tool_uses: vec![],
            turn_end_reason: TurnEndReason::EndTurn,
            usage: None,
        }]);
        let mut agent = Agent::new(model, harness, Arc::new(NullEventSink));
        agent
            .interact(
                vec![Content::Text { text: "hi".into() }],
                crate::core::CancelToken::new(),
            )
            .await
            .expect("turn should succeed");
        assert_eq!(agent.history().len(), 2);
    }

    /// Model whose every generate fails with an oversized-request error.
    struct TooLargeModel;

    impl GenerativeModel for TooLargeModel {
        fn generate(
            &self,
            _input: &[Message],
        ) -> crate::core::AsyncStream<Result<MessagePart, GenerateError>> {
            Box::pin(stream::once(async {
                Err(GenerateError::RequestTooLargeError("42 MiB".into()))
            }))
        }
    }

    /// An oversized request is a property of the history, so the top-level
    /// error must say the last message has to come out — not "try again".
    #[tokio::test]
    async fn oversized_request_reports_omit_last_message() {
        let harness = Harness::local_with_services(vec![]);
        let mut agent = Agent::new(Arc::new(TooLargeModel), harness, Arc::new(NullEventSink));
        let err = agent
            .interact(
                vec![Content::Image {
                    source: "data:image/png;base64,AAAA".into(),
                }],
                crate::core::CancelToken::new(),
            )
            .await
            .expect_err("oversized request should fail");

        assert_eq!(err.recovery(), Recovery::OmitLastMessage);

        // Rewinding takes the offending turn back out and returns its content,
        // leaving a history the provider will accept again.
        let dropped = agent.rewind_last_user_turn().expect("user turn to rewind");
        assert!(matches!(dropped[0], Content::Image { .. }));
        assert!(agent.history().is_empty());
        assert!(agent.rewind_last_user_turn().is_none());
    }

    #[test]
    fn generate_errors_other_than_size_are_retryable() {
        assert_eq!(
            GenerateError::ExecutionError("provider 500".into()).recovery(),
            Recovery::Retry
        );
        assert_eq!(AgentInteractionError::Cancelled.recovery(), Recovery::Retry);
    }

    /// Rewinding mid-turn drops the assistant/tool messages that followed the
    /// user message too — the remaining prefix must end where the *previous*
    /// turn ended, or the next request is malformed.
    #[tokio::test]
    async fn rewind_drops_the_whole_turn_and_keeps_earlier_ones() {
        let harness = Harness::local_with_services(vec![]);
        let model = ScriptedModel::new(vec![]);
        let mut agent = Agent::new(model, harness, Arc::new(NullEventSink));
        agent.set_history(vec![
            Message::UserMessage {
                content: vec![Content::Text {
                    text: "first".into(),
                }],
            },
            Message::AssistantMessage {
                content: vec![Content::Text { text: "ok".into() }],
                tool_uses: vec![],
                turn_end_reason: Some(TurnEndReason::EndTurn),
            },
            Message::UserMessage {
                content: vec![Content::Text {
                    text: "second".into(),
                }],
            },
            Message::AssistantMessage {
                content: vec![],
                tool_uses: vec![ToolUse {
                    id: "call_1".into(),
                    name: "noop".into(),
                    input: json!({}),
                }],
                turn_end_reason: Some(TurnEndReason::ToolUse),
            },
            Message::ToolResults {
                tool_use_results: vec![ToolResult::text("done").with_id("call_1")],
            },
        ]);

        let dropped = agent.rewind_last_user_turn().expect("user turn to rewind");
        assert!(matches!(&dropped[0], Content::Text { text } if text == "second"));

        let history = agent.history();
        assert_eq!(history.len(), 2);
        assert!(matches!(history[0], Message::UserMessage { .. }));
        assert!(matches!(history[1], Message::AssistantMessage { .. }));
    }

    /// Simulate crash after tools: persist history, new agent + model resumes and ends turn.
    #[tokio::test]
    async fn resume_after_tools_mid_turn_continues_cleanly() {
        let slow = Arc::new(SlowService {
            name: "slow_a".into(),
            delay: Duration::from_millis(1),
            starts: Arc::new(Mutex::new(Vec::new())),
            ends: Arc::new(Mutex::new(Vec::new())),
        });
        let harness = Harness::local_with_services(vec![slow.clone() as Arc<dyn ToolService>]);
        let model = FailAfterScriptsModel::new(
            vec![GenerateOutput {
                content: vec![],
                tool_uses: vec![ToolUse {
                    id: "call_mid".into(),
                    name: "slow_a".into(),
                    input: json!({}),
                }],
                turn_end_reason: TurnEndReason::ToolUse,
                usage: None,
            }],
            "simulated crash after tools",
        );
        let mut agent = Agent::new(model, harness, Arc::new(NullEventSink));
        let _ = agent
            .interact(
                vec![Content::Text {
                    text: "mid turn".into(),
                }],
                crate::core::CancelToken::new(),
            )
            .await
            .expect_err("fail after tools");

        let snapshot = agent.history().to_vec();
        assert_eq!(snapshot.len(), 3);

        // "Resume": new agent, same well-formed history, model only needs EndTurn.
        let harness2 = Harness::local_with_services(vec![slow as Arc<dyn ToolService>]);
        let resume_model = ScriptedModel::new(vec![GenerateOutput {
            content: vec![Content::Text {
                text: "recovered".into(),
            }],
            tool_uses: vec![],
            turn_end_reason: TurnEndReason::EndTurn,
            usage: None,
        }]);
        let mut resumed = Agent::new(resume_model, harness2, Arc::new(NullEventSink));
        resumed.set_history(snapshot);

        // Continue by interacting with a follow-up user message (CLI would re-prompt);
        // history already has tool_results so a fresh user turn is the normal path.
        // Also verify set_history alone is well-formed for provider requests by
        // checking the model can complete a new turn on top.
        let reply = resumed
            .interact(
                vec![Content::Text {
                    text: "continue".into(),
                }],
                crate::core::CancelToken::new(),
            )
            .await
            .expect("resume interact");
        assert_eq!(reply.len(), 1);
        match &reply[0] {
            Content::Text { text } => assert_eq!(text, "recovered"),
            other => panic!("expected text, got {other:?}"),
        }

        let history = resumed.history();
        // prior 3 + new user + new assistant
        assert_eq!(history.len(), 5);
        assert!(matches!(history[0], Message::UserMessage { .. }));
        assert!(matches!(history[1], Message::AssistantMessage { .. }));
        assert!(matches!(history[2], Message::ToolResults { .. }));
        assert!(matches!(history[3], Message::UserMessage { .. }));
        assert!(matches!(history[4], Message::AssistantMessage { .. }));
    }
}

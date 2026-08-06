//! The agent runtime: one user turn driven to completion against a model and a
//! harness, plus the live [`AgentEvent`] stream a front-end renders.
//!
//! The top layer: it depends on the model drivers, the harness and the session
//! store, and none of them depend on it. [`TraceContext`] is display
//! attribution, so it lives here; the harness takes a bare agent `Uuid`.
//!
//! History well-formedness is the invariant everything else rests on: whatever
//! a turn does — end cleanly, hit a provider error, get cancelled mid-tool, or
//! get truncated mid-tool-call by `max_tokens` — the transcript it leaves behind
//! must be a prefix the provider will accept on the next request. See
//! [`Agent::interact`].

use std::sync::Arc;

mod compact_worker;
pub use compact_worker::{CompactWorkerError, compact_subagent_prompt, run_compact_worker};

use futures::future;

use crate::core::CancelToken;
use crate::generative_model::{
    self, Content, ContentDelta, GenerateError, GenerateOutput, GenerativeModel, Message,
    MessagePart, Recovery, TokenUsage, ToolResult, ToolUse, TurnEndReason, answer_content,
};
use crate::harness::Harness;
use uuid::Uuid;

//
// Event sink — live observability for agent / tool activity
//

/// Attribution carried on every [`AgentEvent`]: which agent produced it, and
/// how deeply nested that agent is.
///
/// A display concern — sinks filter on [`Self::depth`] to show root-agent output
/// and hide nested workers. One type for every agent role; nesting is a number,
/// not a separate event per role.
#[derive(Debug, Clone)]
pub struct TraceContext {
    /// Stable id for this agent session (root or subagent).
    pub agent_id: Uuid,
    /// Nesting depth: root agent is 0; each nested agent is parent depth + 1.
    pub depth: usize,
}

impl Default for TraceContext {
    fn default() -> Self {
        Self {
            agent_id: Uuid::nil(),
            depth: 0,
        }
    }
}

impl TraceContext {
    pub fn root() -> Self {
        Self {
            agent_id: Uuid::new_v4(),
            depth: 0,
        }
    }
}

/// Live events emitted by the agent runtime.
///
/// All ongoing work is attributed via [`TraceContext::agent_id`].
#[derive(Debug, Clone)]
pub enum AgentEvent {
    /// Incremental assistant text (for streaming UX).
    TextDelta {
        text: String,
        context: TraceContext,
    },
    /// Incremental thinking *summary* text (streamed for UI; also stored in history).
    ThinkingDelta {
        text: String,
        context: TraceContext,
    },
    ToolStarted {
        tool_use: ToolUse,
        context: TraceContext,
    },
    TurnFinished {
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

/// The user turn sent to resume a reply that `max_tokens` cut off mid-text.
///
/// A plain "Continue" invites the model to acknowledge the instruction or start
/// the thought over; naming the requirement keeps the seam invisible in the
/// finished answer.
const CONTINUE_PROMPT: &str = "Continue from exactly where you stopped. Do not repeat anything you have already written, \
     and do not acknowledge this message.";

pub struct Agent {
    model: Arc<dyn GenerativeModel>,
    harness: Arc<Harness>,
    sink: Arc<dyn EventSink>,
    context: TraceContext,
    history: Vec<Message>,
    /// Last turn's usage: input = final request's prompt, output = summed
    /// across that turn's generate calls. [`TokenUsage::context_tokens`] adds
    /// the two for the next USER header — the reply is replayed in the next
    /// request, so input alone under-reports the live context.
    last_usage: Option<TokenUsage>,
    /// Context window for the active model (tokens).
    context_window_tokens: u64,
    /// Consecutive `max_tokens` resumes allowed within one turn, from the
    /// active model's `max_truncated_resumes`.
    max_truncated_resumes: u32,
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
            max_truncated_resumes: crate::config::DEFAULT_MAX_TRUNCATED_RESUMES,
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

    /// Set how many consecutive `max_tokens` truncations one turn resumes
    /// through (the active model's `max_truncated_resumes`; `0` never resumes).
    pub fn set_max_truncated_resumes(&mut self, resumes: u32) {
        self.max_truncated_resumes = resumes;
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
        // Consecutive `max_tokens` stops, capped by MAX_TRUNCATED_RESUMES.
        let mut truncations: u32 = 0;

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
                self.last_usage = Some(TokenUsage {
                    output_tokens: turn_output,
                    ..usage
                });
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
            let answered_tool_calls = !tool_uses.is_empty();
            if answered_tool_calls {
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

            // Consecutive-truncation guard; any clean stop clears it.
            if matches!(reason, TurnEndReason::MaxTokens) {
                truncations += 1;
            } else {
                truncations = 0;
            }

            // A `max_tokens` stop resumes rather than ending the turn: without
            // this an overnight run stops mid-task, holding tool results nobody
            // read or a sentence that breaks off mid-word.
            let resume_truncated = matches!(reason, TurnEndReason::MaxTokens)
                && truncations <= self.max_truncated_resumes;

            // How it resumes depends on what the truncated turn left behind. A
            // turn that carried tool calls already ends on their results, so the
            // next request is an ordinary continuation. Truncated *text* ends on
            // the assistant's own cut-off message, and re-sending that is the
            // prefill shape current Anthropic models reject outright — so ask
            // for the rest in a user turn, the one continuation every provider
            // accepts. It is a real message: the provider is sent it, and the
            // transcript shows it.
            if resume_truncated && !answered_tool_calls {
                self.history.push(Message::UserMessage {
                    content: vec![Content::Text {
                        text: CONTINUE_PROMPT.to_string(),
                    }],
                });
                self.emit_checkpoint();
            }

            // `tool_use` is the other stop that continues the turn; everything
            // else hands control back with whatever the model managed to say.
            if !matches!(reason, TurnEndReason::ToolUse) && !resume_truncated {
                self.sink.emit(AgentEvent::TurnFinished {
                    context: self.context.clone(),
                });
                return Ok(content);
            }
        }
    }

    fn finish_cancelled(&self) -> Result<Vec<Content>, AgentInteractionError> {
        self.sink.emit(AgentEvent::TurnFinished {
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
            context: self.context.clone(),
        });
        Err(AgentInteractionError::GenerateError(error))
    }

    async fn dispatch_tool_use(&self, tool_use: ToolUse, cancel: CancelToken) -> ToolResult {
        self.sink.emit(AgentEvent::ToolStarted {
            tool_use: tool_use.clone(),
            context: self.context.clone(),
        });

        let work =
            self.harness
                .clone()
                .dispatch_tool_use(tool_use, self.context.agent_id, cancel.clone());

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
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                match tokio::time::timeout(CANCEL_TOOL_GRACE, &mut work).await {
                    Ok(result) => result,
                    Err(_) => ToolResult::err("cancelled"),
                }
            }
            result = &mut work => result,
        }
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
    use crate::test_support::{
        ScriptedModel, assistant, assistant_tool, result_text, tool_results, user,
    };
    use crate::tool_services::{HostDispatchContext, ToolService};
    use futures::stream;
    use serde_json::json;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

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
                    .push((tool_use.name.clone(), started));
                tokio::time::sleep(self.delay).await;
                let ended = Instant::now();
                self.ends
                    .lock()
                    .unwrap()
                    .push((tool_use.name.clone(), ended));
                ToolResult::text(format!("done:{}", tool_use.name))
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
                        name: "slow_a".into(),
                        input: json!({}),
                    },
                    ToolUse {
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
                assert_eq!(result_text(&tool_use_results[0]), "done:slow_a");
                assert_eq!(result_text(&tool_use_results[1]), "done:slow_b");
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

        // Input side tracks the latest request; output sums the turn; the
        // context estimate carries both (the next request replays the output).
        let usage = agent.last_usage().expect("usage recorded");
        assert_eq!(usage.input_tokens, 1_600);
        assert_eq!(usage.cached_input_tokens, 900);
        assert_eq!(usage.output_tokens, 520);
        assert_eq!(usage.context_tokens(), 2_120);
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
                let text = result_text(&tool_use_results[0]);
                assert!(text.contains("cancelled"), "{text}");
            }
            other => panic!("expected ToolResults, got {other:?}"),
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
        let model = ScriptedModel::new(vec![GenerateOutput {
            content: vec![],
            tool_uses: vec![ToolUse {
                name: "slow_a".into(),
                input: json!({}),
            }],
            turn_end_reason: TurnEndReason::ToolUse,
            usage: None,
        }])
        .then_fail(GenerateError::ExecutionError(
            "provider 500 after tools".into(),
        ));
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
                assert_eq!(tool_uses[0].name, "slow_a");
                assert_eq!(*turn_end_reason, Some(TurnEndReason::ToolUse));
            }
            other => panic!("expected assistant tool_use, got {other:?}"),
        }
        match &history[2] {
            Message::ToolResults { tool_use_results } => {
                assert_eq!(tool_use_results.len(), 1);
                assert!(!tool_use_results[0].is_error);
            }
            other => panic!("expected ToolResults, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn generate_error_before_assistant_keeps_only_user() {
        let harness = Harness::local_with_services(vec![]);
        let model = ScriptedModel::new(vec![]).then_fail(GenerateError::ExecutionError(
            "boom on first generate".into(),
        ));
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
    /// rejects the empty input — an error result, but a well-formed turn, which
    /// the agent then resumes from without waiting for a new user message.
    #[tokio::test]
    async fn max_tokens_mid_tool_call_answers_the_dangling_tool_use_and_resumes() {
        // The standard local services include the real bash tool.
        let harness = Harness::local_with_services(vec![]);
        let model = ScriptedModel::new(vec![
            GenerateOutput {
                content: vec![Content::Text {
                    text: "let me check".into(),
                }],
                tool_uses: vec![ToolUse {
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
        let reply = agent
            .interact(
                vec![Content::Text { text: "hi".into() }],
                crate::core::CancelToken::new(),
            )
            .await
            .expect("turn should resume through the truncation, not error");

        // The truncation is resumed inside the same turn, so the caller gets the
        // continuation rather than the partial answer that preceded the tool call.
        assert!(matches!(&reply[0], Content::Text { text } if text == "ok"));

        // user + assistant(tool_use) + tool_results + assistant — the tool_use is
        // answered, and the resumed generate appended its reply to the same turn.
        assert_eq!(agent.history().len(), 4);
        match &agent.history()[2] {
            Message::ToolResults { tool_use_results } => {
                assert_eq!(tool_use_results.len(), 1);
                assert!(tool_use_results[0].is_error);
                let text = result_text(&tool_use_results[0]);
                assert!(text.contains("empty bash input"), "text={text}");
            }
            other => panic!("expected ToolResults, got {other:?}"),
        }
    }

    /// A `max_tokens` stop with no tool calls leaves history on the assistant's
    /// cut-off message, which providers reject as a prefill. The turn resumes by
    /// asking for the rest in a user turn — the one continuation every provider
    /// accepts — so a truncated sentence finishes instead of dead-ending.
    #[tokio::test]
    async fn max_tokens_without_tool_calls_resumes_with_a_continue_turn() {
        let harness = Harness::local_with_services(vec![]);
        let model = ScriptedModel::new(vec![
            GenerateOutput {
                content: vec![Content::Text {
                    text: "half a sen".into(),
                }],
                tool_uses: vec![],
                turn_end_reason: TurnEndReason::MaxTokens,
                usage: None,
            },
            GenerateOutput {
                content: vec![Content::Text {
                    text: "tence.".into(),
                }],
                tool_uses: vec![],
                turn_end_reason: TurnEndReason::EndTurn,
                usage: None,
            },
        ]);
        let mut agent = Agent::new(model, harness, Arc::new(NullEventSink));
        let reply = agent
            .interact(
                vec![Content::Text { text: "hi".into() }],
                crate::core::CancelToken::new(),
            )
            .await
            .expect("turn should resume through the truncation");

        assert!(matches!(&reply[0], Content::Text { text } if text == "tence."));

        // user + assistant(truncated) + user(continue) + assistant(rest).
        assert_eq!(agent.history().len(), 4);
        match &agent.history()[2] {
            Message::UserMessage { content } => match content.as_slice() {
                [Content::Text { text }] => assert_eq!(text, CONTINUE_PROMPT),
                other => panic!("expected one text block, got {other:?}"),
            },
            other => panic!("expected the continuation user turn, got {other:?}"),
        }
    }

    /// A model whose output cap is too low truncates every turn. Resuming is
    /// capped so an unattended run stops instead of spending the night
    /// re-truncating; the turn still ends cleanly rather than erroring.
    #[tokio::test]
    async fn consecutive_max_tokens_resumes_are_bounded() {
        const CAP: u32 = 2;
        let harness = Harness::local_with_services(vec![]);
        let truncated_with_tool_call = || GenerateOutput {
            content: vec![Content::Text {
                text: "still going".into(),
            }],
            tool_uses: vec![ToolUse {
                name: "bash".into(),
                input: serde_json::json!({}),
            }],
            turn_end_reason: TurnEndReason::MaxTokens,
            usage: None,
        };
        // One more script than the cap can consume, so the surplus proves the
        // loop stopped on the cap rather than on an exhausted script list.
        let scripts = (0..CAP + 2).map(|_| truncated_with_tool_call()).collect();
        let model = ScriptedModel::new(scripts);
        let mut agent = Agent::new(model.clone(), harness, Arc::new(NullEventSink));
        agent.set_max_truncated_resumes(CAP);
        agent
            .interact(
                vec![Content::Text { text: "hi".into() }],
                crate::core::CancelToken::new(),
            )
            .await
            .expect("turn should hand back once the cap is hit, not error");

        // The initial generate plus CAP resumes.
        assert_eq!(model.remaining() as u32, 1);
    }

    /// `max_truncated_resumes = 0` is the opt-out: the turn hands back the
    /// partial answer, exactly as it did before resuming existed.
    #[tokio::test]
    async fn zero_max_truncated_resumes_hands_back_the_partial_answer() {
        let harness = Harness::local_with_services(vec![]);
        let model = ScriptedModel::new(vec![
            GenerateOutput {
                content: vec![Content::Text {
                    text: "half a sen".into(),
                }],
                tool_uses: vec![],
                turn_end_reason: TurnEndReason::MaxTokens,
                usage: None,
            },
            GenerateOutput {
                content: vec![Content::Text {
                    text: "unused".into(),
                }],
                tool_uses: vec![],
                turn_end_reason: TurnEndReason::EndTurn,
                usage: None,
            },
        ]);
        let mut agent = Agent::new(model.clone(), harness, Arc::new(NullEventSink));
        agent.set_max_truncated_resumes(0);
        let reply = agent
            .interact(
                vec![Content::Text { text: "hi".into() }],
                crate::core::CancelToken::new(),
            )
            .await
            .expect("turn should hand back the partial answer");

        assert!(matches!(&reply[0], Content::Text { text } if text == "half a sen"));
        // user + assistant only: no continuation turn, no second generate.
        assert_eq!(agent.history().len(), 2);
        assert_eq!(model.remaining(), 1, "second script must stay unconsumed");
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

    /// An oversized request is a property of the history, so the top-level
    /// error must say the last message has to come out — not "try again".
    /// (The rewind contract itself is proven by
    /// `rewind_drops_the_whole_turn_and_keeps_earlier_ones`.)
    #[tokio::test]
    async fn oversized_request_reports_omit_last_message() {
        let harness = Harness::local_with_services(vec![]);
        let model = ScriptedModel::new(vec![])
            .then_fail(GenerateError::RequestTooLargeError("42 MiB".into()));
        let mut agent = Agent::new(model, harness, Arc::new(NullEventSink));
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
    }

    /// History is well-formed after a cancel, so the same turn can be re-sent.
    /// (Generate-error recovery mapping is proven in `generative_model` tests.)
    #[test]
    fn cancelled_interaction_is_retryable() {
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
            user("first"),
            assistant("ok"),
            user("second"),
            assistant_tool(None, "noop", json!({})),
            tool_results(&["done"]),
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
        // The well-formed mid-turn snapshot a checkpoint would have persisted
        // before the crash: user + assistant(tool_use) + matching tool_results
        // (the shape `generate_error_after_tool_results_keeps_well_formed_history`
        // proves the agent leaves behind).
        let snapshot = vec![
            user("mid turn"),
            assistant_tool(None, "slow_a", json!({})),
            tool_results(&["done:slow_a"]),
        ];

        // "Resume": new agent, same well-formed history, model only needs EndTurn.
        let harness = Harness::local_with_services(vec![]);
        let resume_model = ScriptedModel::new(vec![GenerateOutput {
            content: vec![Content::Text {
                text: "recovered".into(),
            }],
            tool_uses: vec![],
            turn_end_reason: TurnEndReason::EndTurn,
            usage: None,
        }]);
        let mut resumed = Agent::new(resume_model, harness, Arc::new(NullEventSink));
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

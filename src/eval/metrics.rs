use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Instant;

use futures::Stream;
use serde::{Deserialize, Serialize};

use crate::agent::{AgentEvent, EventSink};
use crate::core::AsyncStream;
use crate::generative_model::{
    GenerateError, GenerationEvent, GenerationFailure, GenerativeModel, Message, MessagePart,
    TokenUsage,
};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Metrics {
    pub requests: u64,
    pub compaction_requests: u64,
    pub requests_with_usage: u64,
    pub input_tokens: u64,
    pub cached_input_tokens: u64,
    pub output_tokens: u64,
    pub tool_calls: u64,
    pub tool_errors: u64,
    pub compactions: u64,
    pub request_limit_reached: bool,
}

pub(super) struct Recorder {
    pub metrics: Mutex<Metrics>,
    pub max_requests: u64,
    events: Mutex<Trace>,
}

struct Trace {
    file: std::fs::File,
    sequence: u64,
    started: Instant,
    error: Option<String>,
}

impl Recorder {
    pub fn new(max_requests: u64, events: std::fs::File) -> Arc<Self> {
        Arc::new(Self {
            metrics: Mutex::new(Metrics::default()),
            max_requests,
            events: Mutex::new(Trace {
                file: events,
                sequence: 0,
                started: Instant::now(),
                error: None,
            }),
        })
    }

    pub fn wrap(
        self: &Arc<Self>,
        inner: Arc<dyn GenerativeModel>,
        compaction: bool,
    ) -> Arc<dyn GenerativeModel> {
        Arc::new(MeasuredModel {
            inner,
            recorder: self.clone(),
            compaction,
        })
    }

    pub fn event(&self, mut value: serde_json::Value) {
        use std::io::Write;
        let mut trace = self.events.lock().unwrap();
        trace.sequence += 1;
        value["version"] = 1.into();
        value["sequence"] = trace.sequence.into();
        value["timestamp"] = serde_json::json!(chrono::Utc::now());
        value["elapsed_ms"] = serde_json::json!(trace.started.elapsed().as_millis());
        if let Err(error) = writeln!(trace.file, "{value}") {
            trace.error.get_or_insert_with(|| error.to_string());
        }
    }

    pub fn check_trace(&self) -> Result<(), String> {
        self.events
            .lock()
            .unwrap()
            .error
            .as_ref()
            .map_or(Ok(()), |error| {
                Err(format!("write eval execution trace: {error}"))
            })
    }
}

impl EventSink for Recorder {
    fn emit(&self, event: AgentEvent) {
        match event {
            AgentEvent::ToolStarted { call_id, tool_use, .. } => {
                self.metrics.lock().unwrap().tool_calls += 1;
                self.event(serde_json::json!({"event":"tool_started", "call_id":call_id, "tool":tool_use}));
            },
            AgentEvent::ToolFinished { call_id, tool_use, result, .. } => {
                if result.is_error { self.metrics.lock().unwrap().tool_errors += 1; }
                self.event(serde_json::json!({"event":"tool_finished", "call_id":call_id, "tool":tool_use.name, "is_error":result.is_error, "status":result.status}));
            },
            AgentEvent::Failure { failure, attempt, max_attempts, retry_in, recovery_remaining, .. } => self.event(serde_json::json!({"event":"generation_failed", "attempt":attempt, "max_attempts":max_attempts, "retry_in_ms":retry_in.map(|delay| delay.as_millis()), "recovery_remaining_ms":recovery_remaining.map(|left| left.as_millis()), "error":failure.cause.to_string()})),
            AgentEvent::GenerationFinished { .. } => self.event(serde_json::json!({"event":"generation_accepted"})),
            _ => {},
        }
    }
}

struct MeasuredModel {
    inner: Arc<dyn GenerativeModel>,
    recorder: Arc<Recorder>,
    compaction: bool,
}

impl GenerativeModel for MeasuredModel {
    fn generate(&self, input: &[Message]) -> AsyncStream<GenerationEvent> {
        let mut metrics = self.recorder.metrics.lock().unwrap();
        if metrics.requests >= self.recorder.max_requests {
            metrics.request_limit_reached = true;
            return Box::pin(futures::stream::once(async {
                GenerationEvent::Failure(GenerationFailure::terminal(
                    GenerateError::ExecutionError("eval request limit reached".into()),
                ))
            }));
        }
        metrics.requests += 1;
        let request_id = metrics.requests;
        if self.compaction {
            metrics.compaction_requests += 1;
        }
        drop(metrics);
        self.recorder.event(serde_json::json!({"event":"request_started", "request_id":request_id, "compaction":self.compaction}));
        Box::pin(MeasuredStream {
            inner: self.inner.generate(input),
            recorder: self.recorder.clone(),
            usage: None,
            compaction: self.compaction,
            request_id,
            outcome: "abandoned",
            error: None,
        })
    }
}

struct MeasuredStream {
    inner: AsyncStream<GenerationEvent>,
    recorder: Arc<Recorder>,
    usage: Option<TokenUsage>,
    compaction: bool,
    request_id: u64,
    outcome: &'static str,
    error: Option<String>,
}

impl Stream for MeasuredStream {
    type Item = GenerationEvent;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let item = self.inner.as_mut().poll_next(cx);
        if let Poll::Ready(Some(GenerationEvent::Part(MessagePart::Usage(usage)))) = &item {
            self.usage = Some(self.usage.map_or(*usage, |current| current.merge(*usage)));
        }
        match &item {
            Poll::Ready(Some(GenerationEvent::Failure(failure))) => {
                self.outcome = "failed";
                self.error = Some(failure.cause.to_string());
            }
            Poll::Ready(None) if self.error.is_none() => self.outcome = "finished",
            _ => {}
        }
        item
    }
}

impl Drop for MeasuredStream {
    fn drop(&mut self) {
        if let Some(usage) = self.usage {
            let mut metrics = self.recorder.metrics.lock().unwrap();
            metrics.requests_with_usage += 1;
            metrics.input_tokens += usage.input_tokens;
            metrics.cached_input_tokens += usage.cached_input_tokens;
            metrics.output_tokens += usage.output_tokens;
        }
        self.recorder.event(serde_json::json!({"event":"request_finished", "request_id":self.request_id, "compaction":self.compaction, "outcome":self.outcome, "error":self.error, "usage":self.usage}));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[cfg(target_os = "linux")]
    #[test]
    fn trace_write_failure_is_reported_as_an_evaluator_error() {
        let recorder = Recorder::new(
            1,
            std::fs::OpenOptions::new()
                .write(true)
                .open("/dev/full")
                .unwrap(),
        );
        recorder.event(serde_json::json!({"event":"run_finished"}));
        assert!(
            recorder
                .check_trace()
                .unwrap_err()
                .contains("write eval execution trace")
        );
    }

    #[tokio::test]
    async fn usage_fragments_are_counted_once_and_compaction_shares_the_request_budget() {
        struct Fragmented;
        impl GenerativeModel for Fragmented {
            fn generate(&self, _: &[Message]) -> AsyncStream<GenerationEvent> {
                Box::pin(futures::stream::iter([
                    GenerationEvent::Part(MessagePart::Usage(TokenUsage {
                        input_tokens: 100,
                        cached_input_tokens: 40,
                        output_tokens: 0,
                    })),
                    GenerationEvent::Part(MessagePart::Usage(TokenUsage {
                        input_tokens: 0,
                        cached_input_tokens: 0,
                        output_tokens: 7,
                    })),
                ]))
            }
        }
        let temp = crate::test_support::temp_dir("eval-usage");
        let recorder = Recorder::new(
            2,
            std::fs::File::create(temp.path().join("events")).unwrap(),
        );
        let main = recorder.wrap(Arc::new(Fragmented), false);
        let compact = recorder.wrap(Arc::new(Fragmented), true);
        main.generate(&[]).collect::<Vec<_>>().await;
        {
            let mut partial = compact.generate(&[]);
            partial.next().await;
            // Cancellation can drop a stream after input usage but before output.
        }
        let denied = main.generate(&[]).next().await;
        assert!(matches!(denied, Some(GenerationEvent::Failure(_))));
        let metrics = recorder.metrics.lock().unwrap();
        assert_eq!(metrics.requests, 2);
        assert_eq!(metrics.compaction_requests, 1);
        assert_eq!(metrics.requests_with_usage, 2);
        assert_eq!(metrics.input_tokens, 200);
        assert_eq!(metrics.cached_input_tokens, 80);
        assert_eq!(metrics.output_tokens, 7);
        assert!(metrics.request_limit_reached);
        let events: Vec<serde_json::Value> = std::fs::read_to_string(temp.path().join("events"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events[1]["outcome"], "finished");
        assert_eq!(events[3]["outcome"], "abandoned");
        assert_eq!(events[3]["request_id"], 2);
        assert_eq!(events[3]["compaction"], true);
    }
}

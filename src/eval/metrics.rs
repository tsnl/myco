use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

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
    events: Mutex<std::fs::File>,
}

impl Recorder {
    pub fn new(max_requests: u64, events: std::fs::File) -> Arc<Self> {
        Arc::new(Self {
            metrics: Mutex::new(Metrics::default()),
            max_requests,
            events: Mutex::new(events),
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

    pub fn event(&self, value: serde_json::Value) {
        use std::io::Write;
        let mut file = self.events.lock().unwrap();
        let _ = writeln!(file, "{value}");
    }
}

impl EventSink for Recorder {
    fn emit(&self, event: AgentEvent) {
        match event {
            AgentEvent::ToolStarted { tool_use, .. } => {
                self.metrics.lock().unwrap().tool_calls += 1;
                self.event(serde_json::json!({"event":"tool_started", "tool":tool_use}));
            },
            AgentEvent::ToolFinished { tool_use, result, .. } => {
                if result.is_error { self.metrics.lock().unwrap().tool_errors += 1; }
                self.event(serde_json::json!({"event":"tool_finished", "tool":tool_use.name, "is_error":result.is_error, "status":result.status}));
            },
            AgentEvent::Failure { failure, attempt, .. } => self.event(serde_json::json!({"event":"generation_failed", "attempt":attempt, "error":failure.cause.to_string()})),
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
        if self.compaction {
            metrics.compaction_requests += 1;
        }
        drop(metrics);
        Box::pin(MeasuredStream {
            inner: self.inner.generate(input),
            recorder: self.recorder.clone(),
            usage: None,
            compaction: self.compaction,
        })
    }
}

struct MeasuredStream {
    inner: AsyncStream<GenerationEvent>,
    recorder: Arc<Recorder>,
    usage: Option<TokenUsage>,
    compaction: bool,
}

impl Stream for MeasuredStream {
    type Item = GenerationEvent;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let item = self.inner.as_mut().poll_next(cx);
        if let Poll::Ready(Some(GenerationEvent::Part(MessagePart::Usage(usage)))) = &item {
            self.usage = Some(self.usage.map_or(*usage, |current| current.merge(*usage)));
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
        self.recorder.event(serde_json::json!({"event":"request_finished", "compaction":self.compaction, "usage":self.usage}));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

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
    }
}

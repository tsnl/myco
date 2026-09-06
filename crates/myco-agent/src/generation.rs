//! Generation attempts, retry policy, and cancellation; response parts reach the sink once.

use futures::StreamExt;

use crate::CancelToken;
use myco_model::{
    ContentDelta, GenerateError, GenerateOutput, GenerationEvent, GenerationFailure,
    MessageAccumulator, MessagePart,
};

use super::{Agent, AgentEvent, AgentInteractionError};

pub(super) async fn generate(
    agent: &Agent,
    cancel: CancelToken,
) -> Result<GenerateOutput, AgentInteractionError> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(AgentInteractionError::Cancelled),
        result = generate_attempts(agent) => result.map_err(AgentInteractionError::GenerateError),
    }
}

async fn generate_attempts(agent: &Agent) -> Result<GenerateOutput, GenerateError> {
    let mut attempt = 1;
    loop {
        let failed = match generate_attempt(agent).await {
            Ok(output) => return Ok(output),
            Err(failed) => failed,
        };
        let retry_in = retry_delay(agent, &failed, attempt);
        agent.sink.emit(AgentEvent::Failure {
            failure: failed.failure.clone(),
            attempt,
            max_attempts: agent.retry_policy.max_attempts.max(1),
            retry_in,
            context: agent.context.clone(),
        });
        let Some(delay) = retry_in else {
            return Err(failed.failure.cause);
        };
        tokio::time::sleep(delay).await;
        attempt += 1;
    }
}

struct FailedAttempt {
    failure: GenerationFailure,
    started: bool,
}

fn retry_delay(agent: &Agent, failed: &FailedAttempt, attempt: u32) -> Option<std::time::Duration> {
    (failed.failure.retryable && !failed.started && attempt < agent.retry_policy.max_attempts).then(
        || {
            agent
                .retry_policy
                .backoff(attempt + 1, failed.failure.retry_after)
        },
    )
}

async fn generate_attempt(agent: &Agent) -> Result<GenerateOutput, FailedAttempt> {
    let mut stream = agent.model.generate(&agent.history);
    let mut accumulator = MessageAccumulator::default();
    let mut started = false;
    while let Some(event) = stream.next().await {
        let part = match event {
            GenerationEvent::Part(part) => part,
            GenerationEvent::Failure(failure) => return Err(FailedAttempt { failure, started }),
        };
        started = true;
        accumulator
            .push(&part)
            .map_err(|cause| FailedAttempt::terminal(cause, started))?;
        emit_part(agent, &part);
    }
    accumulator
        .finish()
        .map_err(|cause| FailedAttempt::terminal(cause, started))
}

impl FailedAttempt {
    fn terminal(cause: GenerateError, started: bool) -> Self {
        Self {
            failure: GenerationFailure::terminal(cause),
            started,
        }
    }
}

fn emit_part(agent: &Agent, part: &MessagePart) {
    let context = agent.context.clone();
    match part {
        MessagePart::ContentDelta(ContentDelta::Text { delta, .. }) => {
            agent.sink.emit(AgentEvent::TextDelta {
                text: delta.clone(),
                context,
            });
        }
        MessagePart::ContentDelta(ContentDelta::Thinking { delta, .. }) if !delta.is_empty() => {
            agent.sink.emit(AgentEvent::ThinkingDelta {
                text: delta.clone(),
                context,
            });
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use crate::test_support::TestTools;
    use crate::test_support::user;
    use myco_model::AsyncStream;
    use myco_model::{ContentStart, GenerativeModel, Message, RetryPolicy, TurnEndReason};

    use super::*;

    struct Attempts {
        scripts: Mutex<VecDeque<Vec<GenerationEvent>>>,
        inputs: Mutex<Vec<serde_json::Value>>,
    }

    impl GenerativeModel for Attempts {
        fn generate(&self, input: &[Message]) -> AsyncStream<GenerationEvent> {
            self.inputs
                .lock()
                .unwrap()
                .push(serde_json::to_value(input).unwrap());
            let events = self
                .scripts
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected attempt");
            Box::pin(futures::stream::iter(events))
        }
    }

    #[derive(Default)]
    struct Events {
        events: Mutex<Vec<AgentEvent>>,
        cancel_on_failure: Option<CancelToken>,
    }

    impl crate::EventSink for Events {
        fn emit(&self, event: AgentEvent) {
            if matches!(event, AgentEvent::Failure { .. })
                && let Some(cancel) = &self.cancel_on_failure
            {
                cancel.cancel();
            }
            self.events.lock().unwrap().push(event);
        }
    }

    fn setup(scripts: Vec<Vec<GenerationEvent>>, events: Arc<Events>) -> (Agent, Arc<Attempts>) {
        let model = Arc::new(Attempts {
            scripts: Mutex::new(scripts.into()),
            inputs: Mutex::default(),
        });
        let mut agent = Agent::new(model.clone(), TestTools::new(vec![]), events);
        agent.set_retry_policy(RetryPolicy {
            initial_backoff: Duration::ZERO,
            ..Default::default()
        });
        agent.replace_context(vec![user("task")], None);
        (agent, model)
    }

    fn failure(retry_after: Option<Duration>) -> GenerationEvent {
        GenerationEvent::Failure(GenerationFailure::transient(
            GenerateError::ExecutionError("busy".into()),
            retry_after,
        ))
    }

    fn answer() -> Vec<GenerationEvent> {
        vec![
            MessagePart::MessageStart,
            MessagePart::ContentStart(ContentStart::Text { index: 0 }),
            MessagePart::ContentDelta(ContentDelta::Text {
                index: 0,
                delta: "answer".into(),
            }),
            MessagePart::TurnEndReason(TurnEndReason::EndTurn),
        ]
        .into_iter()
        .map(GenerationEvent::Part)
        .collect()
    }

    #[tokio::test]
    async fn retry_starts_a_fresh_attempt_with_unchanged_history() {
        let events = Arc::new(Events::default());
        let (agent, model) = setup(vec![vec![failure(None)], answer()], events.clone());
        let output = generate(&agent, CancelToken::new())
            .await
            .expect("second attempt succeeds");
        assert!(
            matches!(output.content.as_slice(), [myco_model::Content::Text { text }] if text == "answer")
        );
        let inputs = model.inputs.lock().unwrap();
        assert_eq!(inputs.len(), 2);
        assert_eq!(inputs[0], inputs[1]);
        let events = events.events.lock().unwrap();
        assert!(
            matches!(events.as_slice(), [AgentEvent::Failure { attempt: 1, retry_in: Some(_), context, .. }, AgentEvent::TextDelta { .. }]
            if context.agent_id == agent.context.agent_id)
        );
    }

    #[tokio::test]
    async fn a_retryable_failure_after_a_part_never_replays_the_attempt() {
        let events = Arc::new(Events::default());
        let (agent, model) = setup(
            vec![vec![
                GenerationEvent::Part(MessagePart::MessageStart),
                failure(None),
            ]],
            events.clone(),
        );
        assert!(matches!(
            generate(&agent, CancelToken::new()).await,
            Err(AgentInteractionError::GenerateError(_))
        ));
        assert_eq!(model.inputs.lock().unwrap().len(), 1);
        assert!(matches!(
            events.events.lock().unwrap().as_slice(),
            [AgentEvent::Failure { retry_in: None, .. }]
        ));
    }

    #[tokio::test]
    async fn cancelling_backoff_stops_before_the_next_attempt() {
        let cancel = CancelToken::new();
        let events = Arc::new(Events {
            cancel_on_failure: Some(cancel.clone()),
            ..Default::default()
        });
        let (mut agent, model) = setup(
            vec![vec![failure(Some(Duration::from_secs(60)))]],
            events.clone(),
        );
        agent.set_retry_policy(RetryPolicy {
            max_backoff: Duration::from_secs(1),
            ..Default::default()
        });
        let outcome = tokio::time::timeout(Duration::from_millis(100), generate(&agent, cancel))
            .await
            .expect("cancel must interrupt backoff");
        assert!(matches!(outcome, Err(AgentInteractionError::Cancelled)));
        assert_eq!(model.inputs.lock().unwrap().len(), 1);
        assert!(
            matches!(events.events.lock().unwrap().as_slice(), [AgentEvent::Failure { retry_in: Some(delay), .. }]
            if *delay == Duration::from_secs(1))
        );
    }

    #[tokio::test]
    async fn cancellation_before_generation_does_not_start_an_attempt() {
        let (agent, model) = setup(vec![], Arc::new(Events::default()));
        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(matches!(
            generate(&agent, cancel).await,
            Err(AgentInteractionError::Cancelled)
        ));
        assert!(model.inputs.lock().unwrap().is_empty());
    }
}

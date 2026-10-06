use super::*;

fn prolonged(agent: &mut Agent) {
    agent.set_retry_policy(RetryPolicy {
        max_attempts: 24,
        initial_backoff: Duration::from_secs(5),
        max_elapsed: Some(Duration::from_secs(300)),
        ..Default::default()
    });
}

#[tokio::test(start_paused = true)]
async fn prolonged_outage_recovers_beyond_short_retries_from_one_committed_boundary() {
    let events = Arc::new(Events::default());
    let mut scripts = vec![vec![failure(None)]; 4];
    scripts.push(answer());
    let (mut agent, model) = setup(scripts, events.clone());
    prolonged(&mut agent);
    let started = Instant::now();
    generate(&agent, CancelToken::new()).await.unwrap();
    assert_eq!(started.elapsed(), Duration::from_secs(65));
    let inputs = model.inputs.lock().unwrap();
    assert_eq!(inputs.len(), 5);
    assert!(inputs.iter().all(|input| input == &inputs[0]));
    let budgets: Vec<_> = events
        .events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| {
            if let AgentEvent::Failure {
                attempt,
                max_attempts,
                recovery_remaining,
                ..
            } = event
            {
                assert_eq!(*max_attempts, 24);
                Some((*attempt, recovery_remaining.unwrap().as_secs()))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(budgets, [(1, 300), (2, 295), (3, 285), (4, 265)]);
}

#[tokio::test(start_paused = true)]
async fn provider_minimum_is_never_shortened_to_fit_a_wait_or_elapsed_cap() {
    for (provider_wait, elapsed, expected) in
        [(60, 300, "max_backoff_ms"), (10, 5, "max_elapsed_ms")]
    {
        let events = Arc::new(Events::default());
        let (mut agent, model) = setup(
            vec![vec![failure(Some(Duration::from_secs(provider_wait)))]],
            events.clone(),
        );
        prolonged(&mut agent);
        agent.retry_policy.max_elapsed = Some(Duration::from_secs(elapsed));
        let started = Instant::now();
        let error = generate(&agent, CancelToken::new()).await.unwrap_err();
        assert!(error.to_string().contains(expected), "{error}");
        assert_eq!(started.elapsed(), Duration::ZERO);
        assert_eq!(model.inputs.lock().unwrap().len(), 1);
        assert!(
            events
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|event| matches!(event,
            AgentEvent::Failure { retry_in: None, failure, .. } if !failure.retryable))
        );
    }
}

struct DelayedAttempts {
    scripts: Mutex<VecDeque<(Duration, Vec<GenerationEvent>)>>,
    calls: std::sync::atomic::AtomicU32,
}

impl GenerativeModel for DelayedAttempts {
    fn generate(&self, _: &[Message]) -> AsyncStream<GenerationEvent> {
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (delay, events) = self
            .scripts
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected request");
        Box::pin(
            futures::stream::once(async move {
                tokio::time::sleep(delay).await;
                events
            })
            .flat_map(futures::stream::iter),
        )
    }
}

#[tokio::test(start_paused = true)]
async fn deadline_starts_after_first_failure_and_aborts_an_in_flight_retry() {
    let events = Arc::new(Events::default());
    let model = Arc::new(DelayedAttempts {
        scripts: Mutex::new(VecDeque::from([
            (Duration::from_secs(400), vec![failure(None)]),
            (Duration::from_secs(20), answer()),
        ])),
        calls: Default::default(),
    });
    let mut agent = Agent::new(model.clone(), TestTools::new(vec![]), events.clone());
    agent.replace_context(vec![user("task")], None).unwrap();
    prolonged(&mut agent);
    agent.retry_policy.max_elapsed = Some(Duration::from_secs(10));
    let started = Instant::now();
    let error = agent.run(CancelToken::new()).await.unwrap_err();
    assert!(
        error.to_string().contains("recovery deadline exhausted"),
        "{error}"
    );
    assert!(error.to_string().contains("busy"));
    assert_eq!(started.elapsed(), Duration::from_secs(410));
    assert_eq!(model.calls.load(std::sync::atomic::Ordering::Relaxed), 2);
    assert_eq!(agent.history().len(), 1);
    assert!(agent.state().is_idle());
    assert!(events.events.lock().unwrap().iter().any(|event| matches!(event,
        AgentEvent::Failure { attempt: 2, recovery_remaining: Some(left), retry_in: None, .. } if left.is_zero())));
}

#[tokio::test(start_paused = true)]
async fn cancelling_prolonged_recovery_interrupts_both_wait_and_stream() {
    for (cancel_after, expected_calls) in [(2, 1), (8, 2)] {
        let model = Arc::new(DelayedAttempts {
            scripts: Mutex::new(VecDeque::from([
                (Duration::ZERO, vec![failure(None)]),
                (Duration::from_secs(200), answer()),
            ])),
            calls: Default::default(),
        });
        let mut agent = Agent::new(
            model.clone(),
            TestTools::new(vec![]),
            Arc::new(Events::default()),
        );
        agent.replace_context(vec![user("task")], None).unwrap();
        prolonged(&mut agent);
        let cancel = CancelToken::new();
        let started = Instant::now();
        let (result, ()) = tokio::join!(agent.run(cancel.clone()), async {
            tokio::time::sleep(Duration::from_secs(cancel_after)).await;
            cancel.cancel();
        });
        assert!(matches!(result, Err(AgentInteractionError::Cancelled)));
        assert_eq!(started.elapsed(), Duration::from_secs(cancel_after));
        assert_eq!(
            model.calls.load(std::sync::atomic::Ordering::Relaxed),
            expected_calls
        );
        assert!(agent.state().is_idle());
        assert_eq!(agent.history().len(), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn malformed_drafts_share_one_recovery_budget_and_never_execute_partial_tools() {
    let events = Arc::new(Events::default());
    let (mut agent, model) = setup(vec![tool_round(&["{}", "{"]); 3], events.clone());
    prolonged(&mut agent);
    agent.retry_policy.initial_backoff = Duration::from_secs(2);
    agent.retry_policy.backoff_multiplier = 1.0;
    agent.retry_policy.max_elapsed = Some(Duration::from_secs(5));
    let error = agent.run(CancelToken::new()).await.unwrap_err();
    assert!(error.to_string().contains("max_elapsed_ms"));
    assert_eq!(model.inputs.lock().unwrap().len(), 3);
    assert_eq!(agent.history().len(), 1);
    assert!(
        !events
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, AgentEvent::ToolStarted { .. }))
    );
}

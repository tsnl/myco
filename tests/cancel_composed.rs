//! Composed cancel path: Agent → Harness → in-process HostWorker → BashService.
//!
//! Every layer has its own cancel test; this is the one users actually hit
//! with Ctrl-C. The agent must give the bash service its grace window to kill
//! the exec's whole process group — dropping the dispatch outright would
//! SIGKILL only the `bash -c` leader and orphan grandchildren.

mod test_utils;

use std::sync::Arc;
use std::time::{Duration, Instant};

use myco::generative_model::{Content, GenerateOutput, Message, ToolUse, TurnEndReason};
use myco::harness::Harness;
use myco::session::{Agent, AgentInteractionError, NullEventSink};
use myco::CancelToken;
use test_utils::ScriptedModel;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_during_local_exec_leaves_no_process_group_survivors() {
    let harness = Harness::local_with_services(vec![]);
    // Unique sleep arg so the ps scan can't match other tests' processes.
    let sleep_tag = format!("23.{}", uuid::Uuid::new_v4().as_u128() % 100_000);
    let model = ScriptedModel::new(vec![GenerateOutput {
        content: vec![],
        tool_uses: vec![ToolUse {
            id: "call_exec".into(),
            name: "bash".into(),
            input: serde_json::json!({
                // `bash -c 'sleep <tag>'` puts the sleep in a grandchild of
                // the service's process group leader.
                "command": format!("sleep {sleep_tag}"),
                "timeout_ms": 60_000,
            }),
        }],
        turn_end_reason: TurnEndReason::ToolUse,
        usage: None,
    }]);
    let mut agent = Agent::new(model, harness, Arc::new(NullEventSink));

    let cancel = CancelToken::new();
    let cancel_bg = cancel.clone();
    tokio::spawn(async move {
        // Let the exec actually spawn before cancelling.
        tokio::time::sleep(Duration::from_millis(500)).await;
        cancel_bg.cancel();
    });

    let t0 = Instant::now();
    let err = agent
        .interact(vec![Content::Text { text: "run".into() }], cancel)
        .await
        .expect_err("turn should be cancelled");
    let elapsed = t0.elapsed();
    assert!(matches!(err, AgentInteractionError::Cancelled), "{err:?}");
    assert!(
        elapsed < Duration::from_secs(5),
        "cancel should return within the grace window, took {elapsed:?}"
    );

    // History stays well-formed: user + assistant(tool_use) + tool_results,
    // with an error result recorded for the cancelled exec.
    let history = agent.history();
    assert_eq!(history.len(), 3, "{history:?}");
    match &history[2] {
        Message::ToolResults { tool_use_results } => {
            assert_eq!(tool_use_results.len(), 1);
            assert!(tool_use_results[0].is_error, "{tool_use_results:?}");
        }
        other => panic!("expected ToolResults, got {other:?}"),
    }

    // Give the group kill a moment to reap, then assert no survivors.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let ps = std::process::Command::new("ps")
        .args(["-ax", "-o", "pid=,command="])
        .output()
        .expect("ps");
    let ps_text = String::from_utf8_lossy(&ps.stdout);
    assert!(
        !ps_text
            .lines()
            .any(|l| l.contains(&format!("sleep {sleep_tag}"))),
        "cancelled exec's process group must be dead; still running:\n{ps_text}"
    );
}

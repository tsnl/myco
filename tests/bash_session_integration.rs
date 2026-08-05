//! Integration test: scripted model drives a multi-turn interactive bash session.
//!
//! No live LLM. The model replays a fixed sequence of tool_use turns; the real
//! BashService executes them. The claims here are agent-level: history length
//! and shape, tool-result ids and ordering, and the `format_transcript`
//! rendering. Bash behavior itself (shell state across writes, timeouts, caps)
//! is proven in-process by `bash_service/tests.rs`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use myco::generative_model::GenerateOutput;
use myco::harness::{Harness, HarnessConfig};
use myco::{Agent, NullEventSink};
use myco_api::EntryBody;
use myco_api::{Content, ToolUse, TurnEndReason};
use serde_json::json;

mod test_utils;
use test_utils::{ScriptedModel, format_transcript, tool_text};

/// A scripted turn holding one bash tool call.
fn bash_turn(id: &str, input: serde_json::Value) -> GenerateOutput {
    GenerateOutput {
        content: vec![],
        tool_uses: vec![ToolUse {
            id: id.into(),
            name: "bash".into(),
            input,
        }],
        turn_end_reason: TurnEndReason::ToolUse,
        usage: None,
    }
}

fn turn_end(text: &str) -> GenerateOutput {
    GenerateOutput {
        content: vec![Content::Text { text: text.into() }],
        tool_uses: vec![],
        turn_end_reason: TurnEndReason::EndTurn,
        usage: None,
    }
}

/// Scripted multi-turn interactive shell: start → echo a marker → close →
/// final answer. Asserts wall-clock bounds, transcript shape, tool-result
/// id/order correlation, and stable `format_transcript` markers.
#[tokio::test]
async fn scripted_multi_turn_bash_session_transcript() {
    let session_id = format!("itest-{}", uuid::Uuid::new_v4().as_simple());

    // Real bash via always-on in-process local host.
    let harness = Harness::attach(HarnessConfig::default())
        .await
        .expect("attach local myco host (build with cargo test --bins)");

    // One generate() per agent loop iteration. The agent keeps calling generate
    // until it sees EndTurn, so we script every tool-use step plus a finale.
    let model = ScriptedModel::new(vec![
        bash_turn(
            "t_start",
            json!({
                "action": "start",
                "session_id": session_id,
                "command": "bash --noprofile --norc",
                "idle_ms": 200,
                "timeout_ms": 1000,
            }),
        ),
        bash_turn(
            "t_echo",
            json!({
                "action": "write",
                "session_id": session_id,
                "stdin": "echo marker-from-shell\n",
                "idle_ms": 300,
                "timeout_ms": 1000,
            }),
        ),
        bash_turn(
            "t_close",
            json!({"action": "close", "session_id": session_id}),
        ),
        turn_end("multi-turn bash session ok"),
    ]);

    let mut agent = Agent::new(model.clone(), harness, Arc::new(NullEventSink));

    let t0 = Instant::now();
    let reply = agent
        .interact(
            myco_test_support::tester(),
            vec![Content::Text {
                text: "Drive an interactive shell across multiple turns.".into(),
            }],
            myco::CancelToken::new(),
        )
        .await
        .expect("interact should succeed");
    let elapsed = t0.elapsed();

    // Three short session steps should finish well under any long hang. CI
    // runners can be slow when the suite already ran heavy lib tests first.
    assert!(
        elapsed < Duration::from_secs(60),
        "scripted multi-turn session took too long: {elapsed:?}"
    );

    assert_eq!(reply.len(), 1);
    match &reply[0] {
        Content::Text { text } => assert_eq!(text, "multi-turn bash session ok"),
        other => panic!("expected final text reply, got {other:?}"),
    }
    assert_eq!(
        model.remaining(),
        0,
        "all scripted turns should be consumed"
    );

    let history = agent.history();
    let transcript = format_transcript(history);
    // Always print so failures and --nocapture runs can inspect the full trace.
    eprintln!("---- transcript ----\n{transcript}---- end transcript ----");

    // Expected shape:
    //   [0] user
    //   [1] assistant tool_use (start)   [2] tool_results (start)
    //   [3] assistant tool_use (echo)    [4] tool_results (echo)
    //   [5] assistant tool_use (close)   [6] tool_results (close)
    //   [7] assistant end
    assert_eq!(
        history.len(),
        8,
        "unexpected history length; transcript:\n{transcript}"
    );

    match &history[0].body {
        EntryBody::User { content } => {
            assert!(matches!(&content[0], Content::Text { text } if text.contains("interactive")));
        }
        other => panic!("history[0] should be a user entry: {other:?}"),
    }

    // Tool results come back correlated by id, in scripted order, all ok.
    let mut results: Vec<(&str, String, bool)> = Vec::new();
    for entry in history {
        if let EntryBody::ToolResults { results: rs } = &entry.body {
            for tr in rs {
                results.push((tr.id.as_str(), tool_text(tr), tr.is_error));
            }
        }
    }
    let ids: Vec<&str> = results.iter().map(|(id, _, _)| *id).collect();
    assert_eq!(
        ids,
        ["t_start", "t_echo", "t_close"],
        "transcript:\n{transcript}"
    );
    for (id, text, is_error) in &results {
        assert!(!*is_error, "{id} failed: {text}");
    }
    assert!(
        results[0].1.contains(&session_id),
        "start should echo session_id; got: {}",
        results[0].1
    );
    // The real shell's output must land in the recorded tool result.
    assert!(
        results[1].1.contains("marker-from-shell"),
        "echo output should reach the transcript; got: {}",
        results[1].1
    );

    // Transcript string itself is the inspectable artifact — pin a few stable markers.
    assert!(transcript.contains("tool_use id=t_start name=bash"));
    assert!(transcript.contains("tool_use id=t_echo name=bash"));
    assert!(transcript.contains("tool_result id=t_echo is_error=false"));
    assert!(transcript.contains("marker-from-shell"));
    assert!(transcript.contains("multi-turn bash session ok"));
    assert!(transcript.contains("turn_end_reason: EndTurn"));
}

//! Integration test: scripted model drives a multi-turn interactive bash session.
//!
//! No live LLM. The model replays a fixed sequence of tool_use turns; the real
//! BashService executes them. We then inspect the agent transcript to assert
//! shell state persisted across writes and that session snapshots returned
//! while the process stayed live.

use std::sync::Arc;
use std::time::{Duration, Instant};

use myco::generative_model::{
    Content, GenerateOutput, Message, ToolUse, TurnEndReason,
};
use myco::harness::{Harness, HarnessConfig};
use myco::{Agent, NullEventSink};
use serde_json::json;

mod test_utils;
use test_utils::{format_transcript, ScriptedModel};

fn tool_text(result: &myco::generative_model::ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn bash_tool(id: &str, input: serde_json::Value) -> ToolUse {
    ToolUse {
        id: id.into(),
        name: "bash".into(),
        input,
    }
}

fn turn_tool_use(tool_uses: Vec<ToolUse>) -> GenerateOutput {
    GenerateOutput {
        content: vec![],
        tool_uses,
        turn_end_reason: TurnEndReason::ToolUse,
    }
}

fn turn_end(text: &str) -> GenerateOutput {
    GenerateOutput {
        content: vec![Content::Text {
            text: text.into(),
        }],
        tool_uses: vec![],
        turn_end_reason: TurnEndReason::EndTurn,
    }
}

/// Scripted multi-turn interactive shell:
///   start → export FOO → print FOO → echo again → list → close → final answer
///
/// Asserts wall-clock bounds, tool success, shell state across writes, and a
/// readable transcript shape.
#[tokio::test]
async fn scripted_multi_turn_bash_session_transcript() {
    let session_id = format!("itest-{}", uuid::Uuid::new_v4().as_simple());

    // Real bash via always-on in-process local host.
    let harness = Harness::attach(HarnessConfig {
        enable_subagent: false,
        ..HarnessConfig::default()
    })
    .await
    .expect("attach local myco host (build with cargo test --bins)");

    // One generate() per agent loop iteration. The agent keeps calling generate
    // until it sees EndTurn, so we script every tool-use step plus a finale.
    let model = ScriptedModel::new(vec![
        turn_tool_use(vec![bash_tool(
            "t_start",
            json!({
                "action": "start",
                "session_id": session_id,
                "command": "bash --noprofile --norc",
                "idle_ms": 200,
                "timeout_ms": 1000,
            }),
        )]),
        turn_tool_use(vec![bash_tool(
            "t_export",
            json!({
                "action": "write",
                "session_id": session_id,
                "stdin": "export MYCO_ITEST=alive-across-turns\n",
                "idle_ms": 200,
                "timeout_ms": 1000,
            }),
        )]),
        turn_tool_use(vec![bash_tool(
            "t_print",
            json!({
                "action": "write",
                "session_id": session_id,
                "stdin": "printf 'saw=%s\\n' \"$MYCO_ITEST\"\n",
                "idle_ms": 300,
                "timeout_ms": 1000,
            }),
        )]),
        turn_tool_use(vec![bash_tool(
            "t_echo",
            json!({
                "action": "write",
                "session_id": session_id,
                "stdin": "echo still-here\n",
                "idle_ms": 300,
                "timeout_ms": 1000,
            }),
        )]),
        turn_tool_use(vec![bash_tool(
            "t_list",
            json!({
                "action": "list",
            }),
        )]),
        turn_tool_use(vec![bash_tool(
            "t_close",
            json!({
                "action": "close",
                "session_id": session_id,
            }),
        )]),
        turn_end("multi-turn bash session ok"),
    ]);

    let mut agent = Agent::new(model.clone(), harness, Arc::new(NullEventSink));

    let t0 = Instant::now();
    let reply = agent
        .interact(
            vec![Content::Text {
                text: "Drive an interactive shell across multiple turns.".into(),
            }],
            myco::CancelToken::new(),
        )
        .await
        .expect("interact should succeed");
    let elapsed = t0.elapsed();

    // Six short session polls + close should finish well under any long hang.
    assert!(
        elapsed < Duration::from_secs(15),
        "scripted multi-turn session took too long: {elapsed:?}"
    );

    assert_eq!(reply.len(), 1);
    match &reply[0] {
        Content::Text { text } => assert_eq!(text, "multi-turn bash session ok"),
        other => panic!("expected final text reply, got {other:?}"),
    }
    assert_eq!(model.remaining(), 0, "all scripted turns should be consumed");

    let history = agent.history();
    let transcript = format_transcript(history);
    // Always print so failures and --nocapture runs can inspect the full trace.
    eprintln!("---- transcript ----\n{transcript}---- end transcript ----");

    // Expected shape:
    //   [0] user
    //   [1] assistant tool_use (start)
    //   [2] tool_results (start)
    //   [3] assistant tool_use (export)
    //   [4] tool_results (export)
    //   [5] assistant tool_use (print)
    //   [6] tool_results (print)  ← must contain saw=alive-across-turns
    //   [7] assistant tool_use (echo)
    //   [8] tool_results (echo)   ← still-here
    //   [9] assistant tool_use (list)
    //   [10] tool_results (list)  ← session id
    //   [11] assistant tool_use (close)
    //   [12] tool_results (close)
    //   [13] assistant end
    assert_eq!(
        history.len(),
        14,
        "unexpected history length; transcript:\n{transcript}"
    );

    match &history[0] {
        Message::UserMessage { content } => {
            assert!(matches!(&content[0], Content::Text { text } if text.contains("interactive")));
        }
        other => panic!("history[0] should be user message: {other:?}"),
    }

    // Collect every tool result text for assertions.
    let mut tool_results: Vec<(&str, String, bool)> = Vec::new();
    for msg in history {
        if let Message::ToolResults { tool_use_results } = msg {
            for tr in tool_use_results {
                tool_results.push((tr.id.as_str(), tool_text(tr), tr.is_error));
            }
        }
    }

    assert_eq!(
        tool_results.len(),
        6,
        "expected 6 bash tool results; transcript:\n{transcript}"
    );

    let by_id = |id: &str| {
        tool_results
            .iter()
            .find(|(i, _, _)| *i == id)
            .unwrap_or_else(|| panic!("missing tool result {id}; transcript:\n{transcript}"))
    };

    let (id, text, is_error) = by_id("t_start");
    assert_eq!(*id, "t_start");
    assert!(!*is_error, "start failed: {text}");
    assert!(
        text.contains(&session_id),
        "start should echo session_id; got: {text}"
    );

    let (_, text, is_error) = by_id("t_export");
    assert!(!*is_error, "export write failed: {text}");

    let (_, text, is_error) = by_id("t_print");
    assert!(!*is_error, "print write failed: {text}");
    assert!(
        text.contains("saw=alive-across-turns"),
        "shell state must persist across turns; print result:\n{text}\ntranscript:\n{transcript}"
    );

    let (_, text, is_error) = by_id("t_echo");
    assert!(!*is_error, "echo write failed: {text}");
    assert!(
        text.contains("still-here"),
        "third write should still hit the same shell; got: {text}"
    );

    let (_, text, is_error) = by_id("t_list");
    assert!(!*is_error, "list failed: {text}");
    assert!(
        text.contains(&session_id),
        "list should show the live session before close; got: {text}"
    );

    let (_, text, is_error) = by_id("t_close");
    assert!(!*is_error, "close failed: {text}");
    assert!(
        text.contains("session closed"),
        "close should confirm reaping; got: {text}"
    );

    // Transcript string itself is the inspectable artifact — pin a few stable markers.
    assert!(transcript.contains("tool_use id=t_start name=bash"));
    assert!(transcript.contains("tool_use id=t_print name=bash"));
    assert!(transcript.contains("tool_result id=t_print is_error=false"));
    assert!(transcript.contains("saw=alive-across-turns"));
    assert!(transcript.contains("multi-turn bash session ok"));
    assert!(transcript.contains("turn_end_reason: EndTurn"));
}

//! Regression: same-turn concurrent host tool uses (join_all) must complete.

mod test_utils;

use std::sync::Arc;
use std::time::{Duration, Instant};

use myco::core::CancelToken;
use myco::generative_model::GenerateOutput;
use myco::harness::{Harness, HarnessConfig};
use myco::{Agent, NullEventSink};
use myco_api::EntryBody;
use myco_api::{Content, ToolUse, TurnEndReason};
use serde_json::json;
use test_utils::{ScriptedModel, tool_text};

fn bash_tool(id: &str, command: &str) -> ToolUse {
    ToolUse {
        id: id.into(),
        name: "bash".into(),
        input: json!({"command": command, "timeout_ms": 5000}),
    }
}

/// One scripted model turn: tool calls end the turn with `ToolUse`, a bare
/// text reply ends it with `EndTurn`.
fn scripted_turn(text: &str, tool_uses: Vec<ToolUse>) -> GenerateOutput {
    let turn_end_reason = if tool_uses.is_empty() {
        TurnEndReason::EndTurn
    } else {
        TurnEndReason::ToolUse
    };
    GenerateOutput {
        content: if text.is_empty() {
            vec![]
        } else {
            vec![Content::Text { text: text.into() }]
        },
        tool_uses,
        turn_end_reason,
        usage: None,
    }
}

#[tokio::test]
async fn agent_concurrent_host_bash_tools_complete() {
    let harness = Harness::attach(HarnessConfig::default())
        .await
        .expect("attach local host");

    let model = ScriptedModel::new(vec![
        scripted_turn(
            "",
            vec![
                bash_tool("t1", "sleep 0.2; echo ONE"),
                bash_tool("t2", "sleep 0.2; echo TWO"),
                bash_tool("t3", "printf THREE"),
            ],
        ),
        scripted_turn("done", vec![]),
    ]);

    let mut agent = Agent::new(model, harness, Arc::new(NullEventSink));
    let t0 = Instant::now();
    let reply = tokio::time::timeout(
        Duration::from_secs(20),
        agent.interact(
            myco_test_support::tester(),
            vec![Content::Text {
                text: "run three".into(),
            }],
            CancelToken::new(),
        ),
    )
    .await
    .expect("agent concurrent host tools hung")
    .expect("interact");

    eprintln!("agent concurrent host tools wall={:?}", t0.elapsed());
    assert_eq!(reply.len(), 1);

    // History: user, asst(tool_use), tool_results, asst(end)
    let history = agent.history();
    assert_eq!(history.len(), 4, "history: {history:?}");
    match &history[2].body {
        EntryBody::ToolResults { results } => {
            assert_eq!(results.len(), 3);
            for (i, id) in ["t1", "t2", "t3"].iter().enumerate() {
                assert_eq!(results[i].id, *id);
                assert!(!results[i].is_error, "tool {id} error: {:?}", results[i]);
            }
            let texts: Vec<String> = results.iter().map(tool_text).collect();
            assert!(texts[0].contains("ONE"), "{}", texts[0]);
            assert!(texts[1].contains("TWO"), "{}", texts[1]);
            assert!(texts[2].contains("THREE"), "{}", texts[2]);
        }
        other => panic!("expected ToolResults, got {other:?}"),
    }
}

#[tokio::test]
async fn agent_concurrent_bash_and_editor_complete() {
    let harness = Harness::attach(HarnessConfig::default())
        .await
        .expect("attach local host");

    let tmp = std::env::temp_dir().join(format!("myco-concurrent-edit-{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&tmp);

    let model = ScriptedModel::new(vec![
        scripted_turn(
            "",
            vec![
                bash_tool("b1", "echo from-bash"),
                ToolUse {
                    id: "e1".into(),
                    name: "str_replace_based_edit_tool".into(),
                    input: json!({
                        "command": "create",
                        "path": tmp.to_string_lossy(),
                        "file_text": "hello-from-editor\n"
                    }),
                },
            ],
        ),
        scripted_turn("ok", vec![]),
    ]);

    let mut agent = Agent::new(model, harness, Arc::new(NullEventSink));
    let reply = tokio::time::timeout(
        Duration::from_secs(20),
        agent.interact(
            myco_test_support::tester(),
            vec![Content::Text {
                text: "run both".into(),
            }],
            CancelToken::new(),
        ),
    )
    .await
    .expect("mixed concurrent host tools hung")
    .expect("interact");
    assert_eq!(reply.len(), 1);

    let history = agent.history();
    match &history[2].body {
        EntryBody::ToolResults { results } => {
            assert_eq!(results.len(), 2);
            assert!(!results[0].is_error, "{:?}", results[0]);
            assert!(!results[1].is_error, "{:?}", results[1]);
        }
        other => panic!("expected ToolResults, got {other:?}"),
    }
    let body = std::fs::read_to_string(&tmp).expect("file written");
    assert!(body.contains("hello-from-editor"), "{body}");
    let _ = std::fs::remove_file(&tmp);
}

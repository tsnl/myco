//! Regression: same-turn concurrent host tool uses (join_all) must complete.

use std::time::{Duration, Instant};

use futures::stream;
use myco::core::AsyncStream;
use myco::core::CancelToken;
use myco::generative_model::{
    Content, GenerateError, GenerateOutput, GenerativeModel, Message, MessagePart, ToolUse,
    TurnEndReason,
};
use myco::harness::{Harness, HarnessConfig};
use myco::{Agent, NullEventSink};
use serde_json::json;
use std::sync::Arc;
use std::sync::Mutex;

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
    fn generate(&self, _input: &[Message]) -> AsyncStream<Result<MessagePart, GenerateError>> {
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
                        myco::generative_model::ContentStart::Text { index: i },
                    ));
                    parts.push(MessagePart::ContentDelta(
                        myco::generative_model::ContentDelta::Text {
                            index: i,
                            delta: text.clone(),
                        },
                    ));
                }
                Content::Image { source } => {
                    parts.push(MessagePart::ContentStart(
                        myco::generative_model::ContentStart::Image { index: i },
                    ));
                    parts.push(MessagePart::ContentDelta(
                        myco::generative_model::ContentDelta::Image {
                            index: i,
                            delta: source.clone(),
                        },
                    ));
                }
                Content::Thinking {
                    text,
                    signature,
                    redacted,
                } => {
                    parts.push(MessagePart::ContentStart(
                        myco::generative_model::ContentStart::Thinking {
                            index: i,
                            signature: signature.clone(),
                            redacted: *redacted,
                        },
                    ));
                    if !text.is_empty() && !*redacted {
                        parts.push(MessagePart::ContentDelta(
                            myco::generative_model::ContentDelta::Thinking {
                                index: i,
                                delta: text.clone(),
                            },
                        ));
                    }
                }
            }
        }
        for (i, tu) in output.tool_uses.iter().enumerate() {
            parts.push(MessagePart::ToolUseStart(
                myco::generative_model::ToolUseStart {
                    index: i,
                    id: tu.id.clone(),
                    name: tu.name.clone(),
                },
            ));
            parts.push(MessagePart::ToolUseDelta(
                myco::generative_model::ToolUseDelta {
                    index: i,
                    input_json_delta: tu.input.to_string(),
                },
            ));
        }
        parts.push(MessagePart::TurnEndReason(output.turn_end_reason));
        Box::pin(stream::iter(parts.into_iter().map(Ok)))
    }
}

fn bash_tool(id: &str, command: &str) -> ToolUse {
    ToolUse {
        id: id.into(),
        name: "bash".into(),
        input: json!({"command": command, "timeout_ms": 5000}),
    }
}

#[tokio::test]
async fn agent_concurrent_host_bash_tools_complete() {
    let harness = Harness::attach(HarnessConfig::default())
        .await
        .expect("attach local host");

    let model = ScriptedModel::new(vec![
        GenerateOutput {
            content: vec![],
            tool_uses: vec![
                bash_tool("t1", "sleep 0.2; echo ONE"),
                bash_tool("t2", "sleep 0.2; echo TWO"),
                bash_tool("t3", "printf THREE"),
            ],
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
    let t0 = Instant::now();
    let reply = tokio::time::timeout(
        Duration::from_secs(20),
        agent.interact(
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
    match &history[2] {
        Message::ToolResults { tool_use_results } => {
            assert_eq!(tool_use_results.len(), 3);
            for (i, id) in ["t1", "t2", "t3"].iter().enumerate() {
                assert_eq!(tool_use_results[i].id, *id);
                assert!(
                    !tool_use_results[i].is_error,
                    "tool {id} error: {:?}",
                    tool_use_results[i]
                );
            }
            let texts: Vec<String> = tool_use_results
                .iter()
                .map(|r| {
                    r.content
                        .iter()
                        .filter_map(|c| match c {
                            Content::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("")
                })
                .collect();
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
    let path = tmp.to_string_lossy().to_string();

    let model = ScriptedModel::new(vec![
        GenerateOutput {
            content: vec![],
            tool_uses: vec![
                bash_tool("b1", "echo from-bash"),
                ToolUse {
                    id: "e1".into(),
                    name: "str_replace_based_edit_tool".into(),
                    input: json!({
                        "command": "create",
                        "path": path,
                        "file_text": "hello-from-editor\n"
                    }),
                },
            ],
            turn_end_reason: TurnEndReason::ToolUse,
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
    let reply = tokio::time::timeout(
        Duration::from_secs(20),
        agent.interact(
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
    match &history[2] {
        Message::ToolResults { tool_use_results } => {
            assert_eq!(tool_use_results.len(), 2);
            assert!(!tool_use_results[0].is_error, "{:?}", tool_use_results[0]);
            assert!(!tool_use_results[1].is_error, "{:?}", tool_use_results[1]);
        }
        other => panic!("expected ToolResults, got {other:?}"),
    }
    let body = std::fs::read_to_string(&tmp).expect("file written");
    assert!(body.contains("hello-from-editor"), "{body}");
    let _ = std::fs::remove_file(&tmp);
}

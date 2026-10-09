use std::sync::Arc;

use agent_kernel_experiment::{data::*, run::*, trace::Trace};
use serde_json::json;

fn player(name: &str, private_observation: &str) -> Trace {
    Trace::new(State::new(
        name,
        Principal::Agent(name.into()),
        Context::new(
            name,
            vec![Message::Input {
                author: Principal::System("environment".into()),
                content: vec![Content::Text(private_observation.into())],
                caused_by: None,
            }],
        ),
        Settings {
            model: "policy".into(),
            parameters: json!({}),
            documents: Arc::new(vec![]),
            tools: Arc::new(vec![Tool {
                name: "move".into(),
                description: "play a number".into(),
                schema: json!({}),
                binding: "joint-environment".into(),
            }]),
        },
    ))
}

fn action(trace: &mut Trace, value: i32) -> OperationId {
    let effects = trace.push(Input::Advance).unwrap();
    let Effect::Generate { id, .. } = &effects[0] else {
        panic!()
    };
    let effects = trace
        .push(Input::Generated {
            id: id.clone(),
            result: Ok(Completion {
                reply: Reply {
                    content: vec![],
                    calls: vec![Call {
                        tool: "move".into(),
                        arguments: json!(value),
                    }],
                },
                finish: Finish::Complete,
                tokens: None,
            }),
        })
        .unwrap();
    let Effect::Call { id, .. } = &effects[0] else {
        panic!()
    };
    id.clone()
}

#[test]
fn simultaneous_environment_uses_a_joint_action_without_merging_private_contexts() {
    let mut a = player("a", "private-a");
    let mut b = player("b", "private-b");
    let a_action = action(&mut a, 1);
    // Environment policy supplies the barrier; the controller has no global turn.
    let mut joint_actions = vec![("a", 1)];
    assert!(matches!(a.replay().unwrap().phase, Phase::Acting { .. }));
    let b_action = action(&mut b, 2);
    joint_actions.push(("b", 2));
    let observation = format!("joint actions: {joint_actions:?}");
    for (trace, id) in [(&mut a, a_action), (&mut b, b_action)] {
        trace
            .push(Input::Observed {
                id,
                outcome: Outcome::Succeeded(vec![Content::Text(observation.clone())]),
            })
            .unwrap();
    }
    // Both used their old private observation; neither had the other's hidden input.
    let a_sample = a.samples().unwrap().remove(0);
    let b_sample = b.samples().unwrap().remove(0);
    let a_input = serde_json::to_string(&a_sample.input).unwrap();
    let b_input = serde_json::to_string(&b_sample.input).unwrap();
    assert!(!a_input.contains("private-b"));
    assert!(!b_input.contains("private-a"));
    assert!(!a_input.contains("joint actions"));
    assert!(!b_input.contains("joint actions"));
    assert!(
        serde_json::to_string(&a.replay().unwrap().context)
            .unwrap()
            .contains("joint actions")
    );
    assert!(
        serde_json::to_string(&b.replay().unwrap().context)
            .unwrap()
            .contains("joint actions")
    );
}

#[test]
fn evaluation_labels_do_not_become_policy_input_or_supervised_targets() {
    let mut t = player("a", "choose a move");
    let id = action(&mut t, 1);
    t.push(Input::Observed {
        id,
        outcome: Outcome::Succeeded(vec![Content::Text("world observation".into())]),
    })
    .unwrap();
    let sample = t.samples().unwrap().remove(0);
    let annotation = json!({
        "target": sample.generation, "evaluator": "hidden-state-grader-v2",
        "reward": 0.75, "evidence": "hidden-solution",
    });
    // The exporter selects the model-produced action, not the observation or grader.
    let supervised = json!({"prompt": sample.input, "completion": sample.output.reply});
    let encoded = supervised.to_string();
    assert!(!encoded.contains("hidden-solution"));
    assert!(!encoded.contains("world observation"));
    assert_eq!(supervised["completion"]["calls"][0]["arguments"], 1);
    assert_eq!(annotation["target"]["run"], "a");
    assert_eq!(annotation["reward"], 0.75);
}

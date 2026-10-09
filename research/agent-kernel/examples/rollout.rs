//! A simulated policy and counter produce portable generation records without a session.

use std::sync::Arc;

use agent_kernel_experiment::{control::Control, data::*, run::*, trace::Trace};
use serde_json::json;

fn main() {
    let principal = Principal::Agent("counter-agent".into());
    let control = Control::new("counter-incarnation-1").transfer(Some(principal.clone()));
    let grant = control.grant(&principal).unwrap();
    let context = Context::new(
        "conversation",
        vec![Message::Input {
            author: Principal::Human("operator".into()),
            content: vec![Content::Text(
                "Increment the counter, then report its value.".into(),
            )],
            caused_by: None,
        }],
    );
    let settings = Settings {
        model: "scripted-policy".into(),
        parameters: json!({}),
        documents: Arc::new(vec![Document {
            name: "prelude".into(),
            revision: "1".into(),
            content: vec![Content::Text("Report observed results.".into())],
        }]),
        tools: Arc::new(vec![Tool {
            name: "add".into(),
            description: "add an integer".into(),
            schema: json!({"type":"integer"}),
            binding: "counter-incarnation-1".into(),
        }]),
    };
    let mut trace = Trace::new(State::new("rollout-1", principal, context, settings));
    let mut counter = 0;
    while trace.replay().unwrap().phase == Phase::Ready {
        let effects = trace.push(Input::Advance).unwrap();
        let Effect::Generate { id, input } = &effects[0] else {
            panic!()
        };
        let observed = input
            .context
            .messages
            .iter()
            .find_map(|message| match message {
                Message::Tool {
                    outcome: Outcome::Succeeded(content),
                    ..
                } => Some(content.clone()),
                _ => None,
            });
        let reply = if let Some(content) = observed {
            Reply {
                content,
                calls: vec![],
            }
        } else {
            Reply {
                content: vec![],
                calls: vec![Call {
                    tool: "add".into(),
                    arguments: json!(1),
                }],
            }
        };
        let effects = trace
            .push(Input::Generated {
                id: id.clone(),
                result: Ok(Completion {
                    reply,
                    finish: Finish::Complete,
                    tokens: None,
                }),
            })
            .unwrap();
        for effect in effects {
            let Effect::Call {
                id,
                principal,
                invocation,
                binding,
            } = effect
            else {
                panic!()
            };
            assert_eq!(binding, "counter-incarnation-1");
            control.check(&principal, &grant).unwrap();
            counter += invocation.arguments.as_i64().unwrap();
            trace
                .push(Input::Observed {
                    id,
                    outcome: Outcome::Succeeded(vec![Content::Text(counter.to_string())]),
                })
                .unwrap();
        }
    }
    let output = json!({
        "note": "Simulated environment and policy; no live model or training has run.",
        "counter": counter, "phase": trace.replay().unwrap().phase,
        "generations": trace.generations().unwrap(),
    });
    println!("{}", serde_json::to_string_pretty(&output).unwrap());
}

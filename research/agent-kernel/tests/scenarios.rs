use std::sync::Arc;

use agent_kernel_experiment::{control::Control, data::*, run::*, trace::*};
use serde_json::json;

fn text(value: &str) -> Vec<Content> {
    vec![Content::Text(value.into())]
}

fn human(value: &str) -> Message {
    Message::Input {
        author: Principal::Human("user".into()),
        content: text(value),
        caused_by: None,
    }
}

fn settings() -> Settings {
    Settings {
        model: "fixture".into(),
        parameters: json!({"temperature": 0.5}),
        documents: Arc::new(vec![Document {
            name: "prelude".into(),
            revision: "v1".into(),
            content: text("Keep the tests passing."),
        }]),
        tools: Arc::new(vec![Tool {
            name: "counter".into(),
            description: "Change a counter".into(),
            schema: json!({"type":"integer"}),
            binding: "counter/incarnation-1/add".into(),
        }]),
    }
}

fn run(id: &str) -> Trace {
    Trace::new(State::new(
        id,
        Principal::Agent(id.into()),
        Context::new(&format!("{id}-context"), vec![human("work")]),
        settings(),
    ))
}

fn generate(trace: &mut Trace) -> OperationId {
    let effects = trace.push(Input::Advance).unwrap();
    let Effect::Generate { id, .. } = &effects[0] else {
        panic!("expected generation")
    };
    id.clone()
}

fn completion(content: &str, actions: &[i32]) -> Completion {
    Completion {
        finish: Finish::Complete,
        reply: Reply {
            content: text(content),
            calls: actions
                .iter()
                .map(|n| Call {
                    tool: "counter".into(),
                    arguments: json!(n),
                })
                .collect(),
        },
        tokens: None,
    }
}

fn reply(trace: &mut Trace, id: OperationId, content: &str, actions: &[i32]) -> Vec<OperationId> {
    trace
        .push(Input::Generated {
            id,
            result: Ok(completion(content, actions)),
        })
        .unwrap()
        .into_iter()
        .map(|e| match e {
            Effect::Call { id, .. } => id,
            _ => panic!("expected call"),
        })
        .collect()
}

fn observe(trace: &mut Trace, id: &OperationId, value: &str) {
    trace
        .push(Input::Observed {
            id: id.clone(),
            outcome: Outcome::Succeeded(text(value)),
        })
        .unwrap();
}

#[test]
fn observations_record_arrival_order_while_context_uses_call_order() {
    let mut t = run("a");
    let g = generate(&mut t);
    let actions = reply(&mut t, g, "running", &[1, 2]);
    observe(&mut t, &actions[1], "second");
    assert!(matches!(t.replay().unwrap().phase, Phase::Acting { .. }));
    assert!(t.push(Input::Advance).is_err());
    observe(&mut t, &actions[0], "first");
    let next_generation = generate(&mut t);
    reply(&mut t, next_generation, "done", &[]);
    let samples = t.samples().unwrap();
    assert_eq!(samples.len(), 2);
    let messages = &samples[1].input.context.messages;
    assert!(
        matches!(&messages[2], Message::Tool { index: 0, outcome: Outcome::Succeeded(v), .. } if v == &text("first"))
    );
    assert!(
        matches!(&messages[3], Message::Tool { index: 1, outcome: Outcome::Succeeded(v), .. } if v == &text("second"))
    );
    assert!(matches!(&t.frames[2].input, Input::Observed { id, .. } if id == &actions[1]));
}

#[test]
fn duplicate_and_foreign_completions_cannot_dispatch_again() {
    let mut t = run("a");
    let g = generate(&mut t);
    let input = Input::Generated {
        id: g,
        result: Ok(completion("run", &[1])),
    };
    assert_eq!(t.push(input.clone()).unwrap().len(), 1);
    assert!(t.push(input).unwrap().is_empty());
    assert!(!t.frames.last().unwrap().applied);
    let foreign = OperationId {
        run: "b".into(),
        sequence: 2,
    };
    assert!(
        t.push(Input::Observed {
            id: foreign,
            outcome: Outcome::Cancelled
        })
        .unwrap()
        .is_empty()
    );
    assert_eq!(t.replay().unwrap().pending().len(), 1);
    assert_eq!(t.samples().unwrap().len(), 1);
}

#[test]
fn cancelled_generation_is_recorded_but_cannot_act_or_become_a_target() {
    let mut t = run("a");
    let g = generate(&mut t);
    assert_eq!(
        t.push(Input::Stop(End::Cancelled)).unwrap(),
        vec![Effect::Cancel(g.clone())]
    );
    let late = Input::Generated {
        id: g,
        result: Ok(completion("too late", &[10])),
    };
    assert!(t.push(late.clone()).unwrap().is_empty());
    assert_eq!(t.frames.last().unwrap().input, late);
    assert!(t.samples().unwrap().is_empty());
    assert_eq!(t.replay().unwrap().phase, Phase::Finished(End::Cancelled));
}

#[test]
fn human_interjection_supersedes_generation_and_keeps_each_input_exact() {
    let mut t = run("a");
    let old = generate(&mut t);
    t.push(Input::Interject(human("new direction"))).unwrap();
    let current = generate(&mut t);
    assert!(reply(&mut t, old, "stale", &[99]).is_empty());
    reply(&mut t, current, "new answer", &[]);
    let samples = t.samples().unwrap();
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].input.context.messages.len(), 2);
    assert_eq!(samples[0].output.reply.content, text("new answer"));
    let Effect::Generate { input, .. } = &t.frames[0].effects[0] else {
        panic!()
    };
    assert_eq!(input.context.messages.len(), 1);
}

#[test]
fn cancellation_request_preserves_actual_tool_outcomes_until_settled() {
    let mut t = run("a");
    let g = generate(&mut t);
    let calls = reply(&mut t, g, "mutating", &[1, 2]);
    t.push(Input::Stop(End::Cancelled)).unwrap();
    assert!(matches!(t.replay().unwrap().phase, Phase::Acting { .. }));
    observe(&mut t, &calls[0], "mutation already happened");
    t.push(Input::Observed {
        id: calls[1].clone(),
        outcome: Outcome::Unknown("worker lost".into()),
    })
    .unwrap();
    let end = t.replay().unwrap();
    assert_eq!(end.phase, Phase::Finished(End::Cancelled));
    assert!(matches!(
        &end.context.messages[2],
        Message::Tool {
            outcome: Outcome::Succeeded(_),
            ..
        }
    ));
    assert!(matches!(
        &end.context.messages[3],
        Message::Tool {
            outcome: Outcome::Unknown(_),
            ..
        }
    ));
}

#[test]
fn interjection_during_tools_waits_for_results_then_continues() {
    let mut t = run("a");
    let g = generate(&mut t);
    let calls = reply(&mut t, g, "mutating", &[1]);
    t.push(Input::Interject(human("stop changing it"))).unwrap();
    assert!(t.push(Input::Advance).is_err());
    observe(&mut t, &calls[0], "changed before cancellation");
    let messages = t.replay().unwrap().context.messages;
    assert!(matches!(&messages[2], Message::Tool { .. }));
    assert_eq!(messages[3], human("stop changing it"));
    generate(&mut t);
}

#[test]
fn failures_and_retries_have_distinct_attempts_without_training_on_partial_text() {
    let mut t = run("a");
    let first = generate(&mut t);
    t.push(Input::Generated {
        id: first.clone(),
        result: Err(Failure {
            reason: "stream disconnected".into(),
            partial: text("incomplete"),
        }),
    })
    .unwrap();
    assert!(t.samples().unwrap().is_empty());
    t.push(Input::Retry).unwrap();
    let second = generate(&mut t);
    assert_ne!(first, second);
    reply(&mut t, second, "complete", &[]);
    let sample = t.samples().unwrap().remove(0);
    assert_eq!(sample.input.context.messages.len(), 1);
    assert_eq!(t.frames.len(), 5);
}

#[test]
fn compaction_keeps_old_observations_and_does_not_own_live_resources() {
    let mut t = run("a");
    let mut counter = 0;
    let g = generate(&mut t);
    let calls = reply(&mut t, g, "mutating", &[4]);
    counter += 4;
    observe(&mut t, &calls[0], "4");
    let old = t.replay().unwrap().context;
    let compacted = old.derive(
        "compacted",
        vec![Message::Summary {
            source: old.at.clone(),
            producer: OperationId {
                run: "summarizer".into(),
                sequence: 1,
            },
            content: text("Counter holds 4."),
        }],
    );
    t.push(Input::InstallContext(compacted)).unwrap();
    let g = generate(&mut t);
    let calls = reply(&mut t, g, "mutating again", &[3]);
    counter += 3;
    observe(&mut t, &calls[0], "7");
    assert_eq!(counter, 7);
    assert!(
        matches!(&old.messages[2], Message::Tool { outcome: Outcome::Succeeded(v), .. } if v == &text("4"))
    );
    let state = t.replay().unwrap();
    assert_eq!(state.context.derived_from, Some(old.at.clone()));
    assert_ne!(state.context.at.lineage, old.at.lineage);
}

#[test]
fn compaction_cannot_install_against_another_revision_or_during_effects() {
    let mut t = run("a");
    let old = t.replay().unwrap().context;
    let replacement = old.derive("new", vec![human("summary")]);
    t.push(Input::Interject(human("new fact"))).unwrap();
    assert!(t.push(Input::InstallContext(replacement)).is_err());
    let state = t.replay().unwrap();
    let replacement = state.context.derive("new", vec![human("summary")]);
    generate(&mut t);
    assert!(t.push(Input::InstallContext(replacement)).is_err());
}

#[test]
fn takeover_and_return_does_not_revalidate_old_commands() {
    let agent = Principal::Agent("a".into());
    let human = Principal::Human("u".into());
    let original = Control::new("shell-1").transfer(Some(agent.clone()));
    let queued = original.grant(&agent).unwrap();
    let human_control = original.transfer(Some(human));
    let returned = human_control.transfer(Some(agent.clone()));
    assert!(returned.check(&agent, &queued).is_err());
    let fresh = returned.grant(&agent).unwrap();
    assert!(returned.check(&agent, &fresh).is_ok());
    assert!(
        Control::new("shell-2")
            .transfer(Some(agent.clone()))
            .check(&agent, &queued)
            .is_err()
    );
}

#[test]
fn control_grants_do_not_supply_the_callers_identity() {
    let a = Principal::Agent("a".into());
    let b = Principal::Agent("b".into());
    let control = Control::new("shared").transfer(Some(a.clone()));
    assert!(control.grant(&b).is_err());
    assert!(control.check(&b, &control.grant(&a).unwrap()).is_err());
}

#[test]
fn two_agents_share_a_resource_without_sharing_context_or_operation_identity() {
    let mut a = run("a");
    let mut b = run("b");
    let ga = generate(&mut a);
    let gb = generate(&mut b);
    let ca = reply(&mut a, ga, "a adds", &[1]).remove(0);
    let cb = reply(&mut b, gb, "b adds", &[2]).remove(0);
    assert_ne!(ca, cb);
    let actor_a = a.initial.principal.clone();
    let actor_b = b.initial.principal.clone();
    let control = Control::new("counter").transfer(Some(actor_a.clone()));
    let grant = control.grant(&actor_a).unwrap();
    assert!(control.check(&actor_a, &grant).is_ok());
    observe(&mut a, &ca, "1");
    assert!(control.grant(&actor_b).is_err());
    b.push(Input::Observed {
        id: cb,
        outcome: Outcome::Failed("held by a".into()),
    })
    .unwrap();
    assert_eq!(a.replay().unwrap().context.messages.len(), 3);
    assert_eq!(b.replay().unwrap().context.messages.len(), 3);
    assert_ne!(a.replay().unwrap().context, b.replay().unwrap().context);
}

#[test]
fn json_roundtrip_preserves_replay_and_multimodal_inputs() {
    let mut t = run("a");
    t.push(Input::Interject(Message::Input {
        author: Principal::Human("u".into()),
        content: vec![Content::Blob {
            digest: "sha256:fixture".into(),
            media_type: "image/png".into(),
        }],
        caused_by: None,
    }))
    .unwrap();
    let g = generate(&mut t);
    reply(&mut t, g, "image answer", &[]);
    let json = serde_json::to_vec(&t).unwrap();
    let decoded: Trace = serde_json::from_slice(&json).unwrap();
    assert_eq!(decoded, t);
    assert_eq!(decoded.replay().unwrap(), t.replay().unwrap());
    assert_eq!(decoded.samples().unwrap(), t.samples().unwrap());
}

#[test]
fn replay_rejects_changed_controller_decisions_and_unknown_versions() {
    let mut t = run("a");
    generate(&mut t);
    let mut changed = t.clone();
    changed.frames[0].effects.clear();
    assert!(changed.replay().is_err());
    t.version += 1;
    assert!(t.replay().is_err());
}

#[test]
fn absent_or_malformed_token_evidence_cannot_masquerade_as_rl_data() {
    let mut t = run("a");
    let g = generate(&mut t);
    reply(&mut t, g, "answer", &[]);
    let mut sample = t.samples().unwrap().remove(0);
    assert!(sample.token_evidence().is_err());
    sample.output.tokens = Some(Tokens {
        policy_revision: "weights-7".into(),
        tokenizer: "tokenizer-3".into(),
        template: "template-2".into(),
        logprob_convention: "post-temperature-and-top-p".into(),
        prompt: vec![4, 5],
        completion: vec![10, 11],
        logprobs: vec![-0.5, -0.2],
    });
    assert_eq!(sample.token_evidence().unwrap().completion, vec![10, 11]);
    sample.output.tokens.as_mut().unwrap().logprobs.pop();
    assert!(sample.token_evidence().is_err());
}

#[test]
fn current_prelude_edits_do_not_change_historical_model_inputs() {
    let mut t = run("a");
    let g = generate(&mut t);
    reply(&mut t, g, "answer", &[]);
    let mut current = settings();
    let document = &mut Arc::make_mut(&mut current.documents)[0];
    document.revision = "v2".into();
    document.content = text("A new instruction.");
    let sample = t.samples().unwrap().remove(0);
    assert_eq!(sample.input.settings.documents[0].revision, "v1");
    assert_ne!(sample.input.settings.documents, current.documents);
}

#[test]
fn a_model_answer_and_a_budget_stop_are_distinct_and_neither_claims_task_success() {
    let mut answered = run("a");
    let g = generate(&mut answered);
    reply(&mut answered, g, "I think I am done", &[]);
    let mut stopped = run("b");
    generate(&mut stopped);
    stopped.push(Input::Stop(End::Budget)).unwrap();
    assert_eq!(
        answered.replay().unwrap().phase,
        Phase::Finished(End::Answered)
    );
    assert_eq!(
        stopped.replay().unwrap().phase,
        Phase::Finished(End::Budget)
    );
}

#[test]
fn output_limit_preserves_the_attempt_without_claiming_a_finished_answer() {
    let mut t = run("a");
    let g = generate(&mut t);
    let mut partial = completion("unfinished", &[1]);
    partial.finish = Finish::Limit;
    assert!(
        t.push(Input::Generated {
            id: g,
            result: Ok(partial.clone())
        })
        .unwrap()
        .is_empty()
    );
    assert!(matches!(t.replay().unwrap().phase, Phase::Failed(_)));
    assert!(t.samples().unwrap().is_empty());
    assert_eq!(t.generations().unwrap()[0].result, Some(Ok(partial)));
}

#[test]
fn a_context_projection_cannot_orphan_or_duplicate_tool_results() {
    let g = OperationId {
        run: "a".into(),
        sequence: 1,
    };
    let assistant = Message::Assistant {
        generation: g.clone(),
        reply: completion("", &[1, 2]).reply,
    };
    let first = Message::Tool {
        generation: g.clone(),
        index: 0,
        outcome: Outcome::Cancelled,
    };
    let second = Message::Tool {
        generation: g,
        index: 1,
        outcome: Outcome::Cancelled,
    };
    for messages in [
        vec![first.clone()],
        vec![assistant.clone()],
        vec![assistant.clone(), first.clone(), first.clone()],
        vec![
            assistant.clone(),
            human("interruption"),
            first.clone(),
            second.clone(),
        ],
    ] {
        assert!(Context::new("invalid", messages).validate_ready().is_err());
    }
    assert!(
        Context::new("valid", vec![assistant, first, second])
            .validate_ready()
            .is_ok()
    );
}

#[test]
fn control_and_observation_revisions_protect_different_invariants() {
    let agent = Principal::Agent("a".into());
    let control = Control::new("document-1").transfer(Some(agent.clone()));
    let grant = control.grant(&agent).unwrap();
    let observation = (0, String::from("old bytes"));
    // A background producer changes the resource while authority stays the same.
    let current = (1, String::from("new bytes"));
    assert!(control.check(&agent, &grant).is_ok());
    let replace = |expected_revision| {
        control.check(&agent, &grant)?;
        if expected_revision != current.0 {
            return Err("observation is stale");
        }
        Ok(())
    };
    assert!(replace(observation.0).is_err());
    assert!(replace(current.0).is_ok());
    assert_eq!(observation.1, "old bytes");
}

#[test]
fn the_same_agent_can_run_again_without_reacquiring_its_live_resource() {
    let actor = Principal::Agent("persistent-agent".into());
    let control = Control::new("shell-1").transfer(Some(actor.clone()));
    let grant = control.grant(&actor).unwrap();
    let mut first = Trace::new(State::new(
        "run-1",
        actor.clone(),
        Context::new("c", vec![]),
        settings(),
    ));
    let g = generate(&mut first);
    reply(&mut first, g, "done", &[]);
    let previous = first.replay().unwrap();
    let mut next = Trace::new(State::new("run-2", actor, previous.context, settings()));
    generate(&mut next);
    assert!(control.check(&next.initial.principal, &grant).is_ok());
    assert_ne!(first.initial.run, next.initial.run);
}

#[test]
fn every_authority_epoch_invalidates_all_previous_grants() {
    let actors = [
        Principal::Agent("a".into()),
        Principal::Agent("b".into()),
        Principal::Human("u".into()),
    ];
    // Enumerate 3^5 handoff histories, including repeated holders and ABA patterns.
    for mut choices in 0..243 {
        let mut control = Control::new("resource");
        let mut previous = vec![];
        for _ in 0..5 {
            let actor = actors[choices % 3].clone();
            choices /= 3;
            control = control.transfer(Some(actor.clone()));
            for (old_actor, grant) in &previous {
                assert!(control.check(old_actor, grant).is_err());
            }
            let grant = control.grant(&actor).unwrap();
            assert!(control.check(&actor, &grant).is_ok());
            previous.push((actor, grant));
        }
    }
}

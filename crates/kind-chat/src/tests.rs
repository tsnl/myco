//! Driven through the pool, not the struct: attribution comes from the bus,
//! and that path is the claim under test. Turns are scripted
//! ([`ScriptedModel`]) or parked ([`HangingModel`]) — no network anywhere.

use std::sync::Arc;

use myco_instance::{Pool, Principal, VerbError};
use myco_models::test_support::{HangingModel, ScriptedModel};
use myco_models::{CatalogFile, resolve_catalog};
use serde_json::{Value, json};

use super::{ChatKind, ModelFactory};

fn ada() -> Principal {
    Principal::Human("ada".into())
}

fn pool_with_chat(args: Value) -> (Pool, String) {
    let pool = Pool::new();
    pool.register(Arc::new(ChatKind::transcript_only(pool.clone())));
    let info = pool.create(&ada(), "chat", "", "", args).expect("create");
    (pool, info.id)
}

/// A one-model catalog ("fake") whose backend is never reached — the
/// factory injects the test double instead.
fn fake_catalog() -> myco_models::ModelCatalog {
    let file: CatalogFile = toml::from_str(
        r#"
[models.fake]
protocol = "openai-completions"
base_url = "http://127.0.0.1:9/v1"
context_window = 100000
"#,
    )
    .expect("catalog parses");
    resolve_catalog(&file, &|_| None, &|_| Err("no files".into())).expect("resolves")
}

fn modeled_pool(factory: ModelFactory) -> (Pool, String) {
    let pool = Pool::new();
    pool.register(Arc::new(ChatKind::with_factory(
        pool.clone(),
        fake_catalog(),
        Some("fake".into()),
        factory,
    )));
    // The dispatcher's workhorse: tool turns create tty instances.
    pool.register(Arc::new(myco_kind_tty::TtyKind));
    let info = pool
        .create(&ada(), "chat", "", "", Value::Null)
        .expect("create");
    (pool, info.id)
}

/// Wait until the chat's assistant entry count with a set `turn_end`
/// reaches `n`, driven by the watermark (never a sleep).
async fn wait_for_settled_turns(pool: &Pool, id: &str, n: usize) -> Value {
    let mut mark = 0;
    loop {
        let tail = pool
            .call(&ada(), id, "tail", Value::Null)
            .await
            .expect("tail");
        let settled = tail["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["t"] == "assistant" && e.get("turn_end").is_some())
            .count();
        if settled >= n {
            return tail;
        }
        mark = pool.changed(id, mark).await.expect("changed");
    }
}

#[tokio::test]
async fn posts_are_attributed_to_the_bus_principal_and_bump_the_watermark() {
    let (pool, id) = pool_with_chat(Value::Null);
    let before = pool.watermark(&id).expect("watermark");

    let posted = pool
        .call(&ada(), &id, "post", json!({"text": "hello"}))
        .await
        .expect("post");
    assert_eq!(posted["seq"], 0);
    assert_eq!(posted["author"]["kind"], "human");
    assert_eq!(posted["author"]["id"], "ada");

    // A different principal posts to the same chat — no seat needed, the
    // transcript is multiplayer; only the attribution differs.
    let agent = Principal::Agent(id.clone());
    pool.call(&agent, &id, "post", json!({"text": "hi ada"}))
        .await
        .expect("agent post");

    let after = pool.watermark(&id).expect("watermark");
    assert!(after > before, "posts wake watchers");

    let tail = pool
        .call(&ada(), &id, "tail", Value::Null)
        .await
        .expect("tail");
    let entries = tail["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["author"]["kind"], "human");
    assert_eq!(entries[1]["author"]["kind"], "agent");
    assert_eq!(entries[1]["t"], "message");
}

/// The author is the framework's fact: nothing in the arguments can forge
/// it — an `author` field in the args is simply ignored.
#[tokio::test]
async fn the_arguments_cannot_forge_an_author() {
    let (pool, id) = pool_with_chat(Value::Null);
    let posted = pool
        .call(
            &ada(),
            &id,
            "post",
            json!({"text": "x", "author": {"kind": "agent", "id": "mallory"}}),
        )
        .await
        .expect("post");
    assert_eq!(posted["author"]["id"], "ada");
}

#[tokio::test]
async fn tail_cursors_in_dense_sequence_numbers_with_a_budget() {
    let (pool, id) = pool_with_chat(Value::Null);
    for i in 0..5 {
        pool.call(&ada(), &id, "post", json!({"text": format!("m{i}")}))
            .await
            .expect("post");
    }

    let first = pool
        .call(&ada(), &id, "tail", json!({"from": 0, "max_entries": 2}))
        .await
        .expect("tail");
    assert_eq!(first["entries"].as_array().unwrap().len(), 2);
    assert_eq!(first["next"], 2);
    assert_eq!(first["len"], 5);

    // Resume from `next`: the cursor contract, no overlap, no gap.
    let rest = pool
        .call(&ada(), &id, "tail", json!({"from": 2}))
        .await
        .expect("tail");
    let entries = rest["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0]["seq"], 2);
    assert_eq!(rest["next"], 5);

    // A cursor past the end is clamped, answers empty, and is not an error
    // (a quiet chat is not a broken one).
    let past = pool
        .call(&ada(), &id, "tail", json!({"from": 99}))
        .await
        .expect("tail");
    assert_eq!(past["entries"].as_array().unwrap().len(), 0);
    assert_eq!(past["next"], 5);
}

#[tokio::test]
async fn text_is_the_plain_transcript() {
    let (pool, id) = pool_with_chat(Value::Null);
    pool.call(&ada(), &id, "post", json!({"text": "hello"}))
        .await
        .expect("post");
    pool.call(
        &Principal::Agent(id.clone()),
        &id,
        "post",
        json!({"text": "hi"}),
    )
    .await
    .expect("post");

    let text = pool
        .call(&ada(), &id, "text", Value::Null)
        .await
        .expect("text");
    assert_eq!(
        text["text"].as_str().unwrap(),
        format!("ada: hello\nagent:{id}: hi\n")
    );
}

#[tokio::test]
async fn a_subagent_is_a_chat_with_a_parent_and_nothing_more() {
    let (pool, parent_id) = pool_with_chat(Value::Null);
    let child = pool
        .create(&ada(), "chat", "", "", json!({"parent": parent_id}))
        .expect("create child");

    let about = pool
        .call(&ada(), &child.id, "about", Value::Null)
        .await
        .expect("about");
    assert_eq!(about["parent"], json!(parent_id));

    let about_parent = pool
        .call(&ada(), &parent_id, "about", Value::Null)
        .await
        .expect("about");
    assert_eq!(about_parent["parent"], Value::Null);
}

#[tokio::test]
async fn empty_posts_and_bad_parents_are_refused_by_name() {
    let (pool, id) = pool_with_chat(Value::Null);
    for bad in [json!({}), json!({"text": ""}), json!({"text": "   "})] {
        let err = pool.call(&ada(), &id, "post", bad).await.unwrap_err();
        assert!(matches!(err, VerbError::BadArgs { .. }), "{err}");
    }

    let err = pool
        .create(&ada(), "chat", "", "", json!({"parent": 7}))
        .unwrap_err();
    assert!(matches!(err, VerbError::BadArgs { .. }), "{err}");
}

// ---------------------------------------------------------------------------
// Turns
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_human_post_starts_a_turn_that_streams_into_the_transcript() {
    let scripted = ScriptedModel::replying(&["hello ada"]);
    let (pool, id) = modeled_pool(Arc::new(move |_| Ok(scripted.clone() as _)));

    pool.call(&ada(), &id, "post", json!({"text": "hi"}))
        .await
        .expect("post");
    let tail = wait_for_settled_turns(&pool, &id, 1).await;

    let entries = tail["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    let reply = &entries[1];
    assert_eq!(reply["t"], "assistant");
    assert_eq!(reply["model"], "fake");
    assert_eq!(reply["author"]["kind"], "agent");
    assert_eq!(reply["turn_end"], json!("EndTurn"));
    assert_eq!(reply["content"][0]["Text"]["text"], "hello ada");

    // The plain transcript shows both sides — the escape hatch holds.
    let text = pool
        .call(&ada(), &id, "text", Value::Null)
        .await
        .expect("text");
    assert!(text["text"].as_str().unwrap().contains("hello ada"));
}

/// The model answers people. Its own entries, other agents' posts, and
/// system posts must not start turns — the loop that would otherwise talk
/// to itself forever.
#[tokio::test]
async fn only_human_posts_trigger_turns() {
    let scripted = ScriptedModel::replying(&["one"]);
    let (pool, id) = modeled_pool(Arc::new(move |_| Ok(scripted.clone() as _)));

    pool.call(&ada(), &id, "post", json!({"text": "hi"}))
        .await
        .expect("post");
    wait_for_settled_turns(&pool, &id, 1).await;

    // An agent post and a system post land as entries, and nothing follows:
    // the scripted model has no outputs left, so a triggered turn would
    // fail loudly as an `error` turn_end.
    pool.call(
        &Principal::Agent("other".into()),
        &id,
        "post",
        json!({"text": "agent aside"}),
    )
    .await
    .expect("agent post");
    pool.call(
        &Principal::System("cron".into()),
        &id,
        "post",
        json!({"text": "tick"}),
    )
    .await
    .expect("system post");

    let about = pool
        .call(&ada(), &id, "about", Value::Null)
        .await
        .expect("about");
    assert_eq!(about["turn_running"], json!(false));
    assert_eq!(about["len"], 4, "two posts appended, no new assistant entry");
}

#[tokio::test]
async fn cancel_aborts_the_turn_and_the_partial_entry_stays_marked() {
    let (pool, id) = modeled_pool(Arc::new(|_| Ok(Arc::new(HangingModel) as _)));

    pool.call(&ada(), &id, "post", json!({"text": "think forever"}))
        .await
        .expect("post");
    // The turn opened its streaming entry; it will never finish on its own.
    let mut mark = 0;
    loop {
        let about = pool
            .call(&ada(), &id, "about", Value::Null)
            .await
            .expect("about");
        if about["len"] == json!(2) {
            break;
        }
        mark = pool.changed(&id, mark).await.expect("changed");
    }

    let cancelled = pool
        .call(&ada(), &id, "cancel", Value::Null)
        .await
        .expect("cancel");
    assert_eq!(cancelled["cancelled"], json!(true));

    let tail = pool
        .call(&ada(), &id, "tail", Value::Null)
        .await
        .expect("tail");
    let reply = &tail["entries"].as_array().unwrap()[1];
    assert_eq!(reply["turn_end"], json!({"Other": "cancelled"}));

    // Cancelling again is a polite no-op, not an error.
    let again = pool
        .call(&ada(), &id, "cancel", Value::Null)
        .await
        .expect("cancel again");
    assert_eq!(again["cancelled"], json!(false));
}

/// An interjection: a human post mid-turn kills the running turn (marked
/// `interrupted`) and starts a fresh one over the longer transcript.
#[tokio::test]
async fn a_post_mid_turn_interrupts_and_restarts() {
    let hanging = Arc::new(HangingModel);
    let (pool, id) = modeled_pool(Arc::new(move |_| Ok(hanging.clone() as _)));

    pool.call(&ada(), &id, "post", json!({"text": "first"}))
        .await
        .expect("post");
    let mut mark = 0;
    loop {
        let about = pool
            .call(&ada(), &id, "about", Value::Null)
            .await
            .expect("about");
        if about["len"] == json!(2) && about["turn_running"] == json!(true) {
            break;
        }
        mark = pool.changed(&id, mark).await.expect("changed");
    }

    pool.call(&ada(), &id, "post", json!({"text": "actually, wait"}))
        .await
        .expect("interject");

    let tail = pool
        .call(&ada(), &id, "tail", Value::Null)
        .await
        .expect("tail");
    let entries = tail["entries"].as_array().unwrap();
    // first post, interrupted turn, second post, and the new turn's entry
    // (opening is async — allow it to not have landed yet).
    assert_eq!(entries[1]["turn_end"], json!({"Other": "interrupted"}));
    assert_eq!(entries[2]["t"], "message");
    let about = pool
        .call(&ada(), &id, "about", Value::Null)
        .await
        .expect("about");
    assert_eq!(about["turn_running"], json!(true), "a fresh turn runs");
}

#[tokio::test]
async fn a_failing_stream_marks_the_entry_instead_of_wedging_the_chat() {
    let scripted = ScriptedModel::new(vec![])
        .then_fail(myco_models::GenerateError::ExecutionError("boom".into()));
    let (pool, id) = modeled_pool(Arc::new(move |_| Ok(scripted.clone() as _)));

    pool.call(&ada(), &id, "post", json!({"text": "hi"}))
        .await
        .expect("post");
    let tail = wait_for_settled_turns(&pool, &id, 1).await;
    let reply = &tail["entries"].as_array().unwrap()[1];
    let end = reply["turn_end"]["Other"].as_str().unwrap();
    assert!(end.starts_with("error:"), "{end}");

    // The chat still answers verbs — a failed turn is an entry, not a wedge.
    pool.call(&ada(), &id, "post", json!({"text": "still there?"}))
        .await
        .expect("post after failure");
}

#[tokio::test]
async fn an_unknown_model_is_refused_at_create_with_the_catalog_listed() {
    let pool = Pool::new();
    pool.register(Arc::new(ChatKind::with_factory(
        pool.clone(),
        fake_catalog(),
        None,
        Arc::new(|_| panic!("factory must not run for an unknown key")),
    )));
    let err = pool
        .create(&ada(), "chat", "", "", json!({"model": "nope"}))
        .unwrap_err();
    match err {
        VerbError::BadArgs { why } => assert!(why.contains("fake"), "{why}"),
        other => panic!("expected BadArgs, got {other}"),
    }
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

use myco_models::{GenerateOutput, ToolUse, TurnEndReason};

fn tool_turn(command: &str, id: &str) -> GenerateOutput {
    GenerateOutput {
        content: vec![],
        tool_uses: vec![ToolUse {
            id: id.into(),
            name: "bash".into(),
            input: json!({"command": command}),
        }],
        turn_end_reason: TurnEndReason::ToolUse,
        usage: None,
    }
}

/// The whole loop: the model calls bash, the dispatcher runs it as a tty
/// on the bus (as the agent principal), the result feeds the next
/// generation, and the workhorse tty is removed afterwards.
#[tokio::test]
async fn a_tool_turn_runs_bash_on_the_bus_and_feeds_the_result_back() {
    let scripted = ScriptedModel::new(vec![
        tool_turn("echo tool-says-hi", "t1"),
        myco_models::test_support::text_output("done"),
    ]);
    let (pool, id) = modeled_pool(Arc::new(move |_| Ok(scripted.clone() as _)));

    pool.call(&ada(), &id, "post", json!({"text": "run it"}))
        .await
        .expect("post");
    let tail = wait_for_settled_turns(&pool, &id, 2).await;
    let entries = tail["entries"].as_array().unwrap();

    // post · assistant(tool_use) · tool_results · assistant(done)
    assert_eq!(entries.len(), 4);
    assert_eq!(entries[1]["t"], "assistant");
    assert_eq!(entries[1]["turn_end"], json!("ToolUse"));
    assert_eq!(entries[1]["tool_uses"][0]["name"], "bash");
    assert_eq!(entries[2]["t"], "tool_results");
    let result_text = entries[2]["results"][0]["content"][0]["Text"]["text"]
        .as_str()
        .unwrap();
    assert!(result_text.contains("tool-says-hi"), "{result_text}");
    assert_eq!(entries[2]["results"][0]["id"], "t1", "paired by id");
    assert_eq!(entries[3]["content"][0]["Text"]["text"], "done");

    // Every model-side entry speaks as the chat's own agent principal.
    for e in &entries[1..] {
        assert_eq!(e["author"], json!({"kind": "agent", "id": id}));
    }

    // The workhorse terminal is gone; only the chat remains.
    let mut mark = 0;
    loop {
        if pool.list(None).len() == 1 {
            break;
        }
        mark = pool.changed(&id, mark).await.expect("changed");
    }
}

#[tokio::test]
async fn an_unknown_tool_answers_with_an_error_result_not_a_wedge() {
    let scripted = ScriptedModel::new(vec![
        GenerateOutput {
            content: vec![],
            tool_uses: vec![ToolUse {
                id: "t1".into(),
                name: "teleport".into(),
                input: json!({}),
            }],
            turn_end_reason: TurnEndReason::ToolUse,
            usage: None,
        },
        myco_models::test_support::text_output("noted"),
    ]);
    let (pool, id) = modeled_pool(Arc::new(move |_| Ok(scripted.clone() as _)));

    pool.call(&ada(), &id, "post", json!({"text": "go"}))
        .await
        .expect("post");
    let tail = wait_for_settled_turns(&pool, &id, 2).await;
    let entries = tail["entries"].as_array().unwrap();
    assert_eq!(entries[2]["results"][0]["is_error"], json!(true));
    let why = entries[2]["results"][0]["content"][0]["Text"]["text"]
        .as_str()
        .unwrap();
    assert!(why.contains("unknown tool"), "{why}");
}

#[tokio::test]
async fn a_timed_out_command_is_killed_and_reports_partial_output() {
    let scripted = ScriptedModel::new(vec![
        tool_turn("echo started; sleep 60", "t1"),
        myco_models::test_support::text_output("gave up"),
    ]);
    // Rewrite the timeout down so the test runs in ~1s.
    let scripted_with_timeout = ScriptedModel::new(vec![
        GenerateOutput {
            content: vec![],
            tool_uses: vec![ToolUse {
                id: "t1".into(),
                name: "bash".into(),
                input: json!({"command": "echo started; sleep 60", "timeout_secs": 1}),
            }],
            turn_end_reason: TurnEndReason::ToolUse,
            usage: None,
        },
        myco_models::test_support::text_output("gave up"),
    ]);
    drop(scripted);
    let (pool, id) = modeled_pool(Arc::new(move |_| Ok(scripted_with_timeout.clone() as _)));

    pool.call(&ada(), &id, "post", json!({"text": "go"}))
        .await
        .expect("post");
    let tail = wait_for_settled_turns(&pool, &id, 2).await;
    let entries = tail["entries"].as_array().unwrap();
    let result = &entries[2]["results"][0];
    assert_eq!(result["is_error"], json!(true));
    let why = result["content"][0]["Text"]["text"].as_str().unwrap();
    assert!(why.contains("timed out"), "{why}");
    assert!(why.contains("started"), "partial output survives: {why}");

    // The killed command's terminal is removed too.
    let mut mark = 0;
    loop {
        if pool.list(None).len() == 1 {
            break;
        }
        mark = pool.changed(&id, mark).await.expect("changed");
    }
}

/// Cancelling mid-command still cleans up: the removal rides a drop guard
/// on the turn task, so an aborted task cannot leak its terminal.
#[tokio::test]
async fn cancel_mid_command_removes_the_workhorse_terminal() {
    let scripted = ScriptedModel::new(vec![tool_turn("sleep 60", "t1")]);
    let (pool, id) = modeled_pool(Arc::new(move |_| Ok(scripted.clone() as _)));

    pool.call(&ada(), &id, "post", json!({"text": "go"}))
        .await
        .expect("post");
    // Wait until the tool's tty exists.
    let mut mark = 0;
    loop {
        if pool.list(None).len() == 2 {
            break;
        }
        mark = pool.changed(&id, mark).await.expect("changed");
    }

    pool.call(&ada(), &id, "cancel", Value::Null)
        .await
        .expect("cancel");
    loop {
        if pool.list(None).len() == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// Attribution in the projection: solo chats read as typed; multiplayer
/// chats prefix every post with its speaker.
#[test]
fn the_projection_attributes_speakers_only_when_there_are_several() {
    use super::{Body, Entry};
    use chrono::Utc;

    let entry = |author: Principal, text: &str| Entry {
        seq: 0,
        at: Utc::now(),
        author,
        body: Body::Message { text: text.into() },
    };

    let solo = super::project(&[entry(ada(), "hello")]);
    match &solo[0] {
        myco_models::Message::UserMessage { content } => {
            assert_eq!(content.len(), 1, "solo chats are left as typed");
        }
        other => panic!("wrong message: {other:?}"),
    }

    let multi = super::project(&[
        entry(ada(), "hello"),
        entry(Principal::Human("grace".into()), "hi"),
    ]);
    match &multi[1] {
        myco_models::Message::UserMessage { content } => {
            assert_eq!(content.len(), 2);
            match &content[0] {
                myco_models::Content::Text { text } => assert_eq!(text, "[grace]"),
                other => panic!("wrong block: {other:?}"),
            }
        }
        other => panic!("wrong message: {other:?}"),
    }
}

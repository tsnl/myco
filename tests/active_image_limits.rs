//! Active model limits travel with calls; shared workers retain session resources.
mod test_utils;

use std::path::Path;
use std::sync::Arc;

use myco::agent::ToolExecutor;
use myco::chat::SessionRunner;
use myco::core::CancelToken;
use myco::generative_model::{ToolResult, ToolUse};
use myco::harness::{Harness, HarnessConfig, HostConfig};
use myco::{
    ActiveSession, Agent, HostDispatchContext, ModelInfo, NullEventSink, Session, SessionRuntime,
};
use serde_json::{Value, json};

async fn runner(harness: Arc<Harness>, cap: u64) -> SessionRunner {
    let session = Session::new(format!("image-model-{cap}"));
    session.save().unwrap();
    let runtime = SessionRuntime::new(harness, ActiveSession::new(session));
    runtime.set_max_image_base64_bytes(cap);
    let agent = Agent::new(
        test_utils::ScriptedModel::new(vec![]),
        runtime.clone(),
        Arc::new(NullEventSink),
    );
    let mut runner = SessionRunner::new(agent, runtime).await.unwrap();
    select(&mut runner, cap).await;
    runner
}

async fn select(runner: &mut SessionRunner, cap: u64) {
    runner
        .set_model(
            test_utils::ScriptedModel::new(vec![]),
            ModelInfo::named(format!("image-model-{cap}")),
        )
        .await
        .unwrap();
    runner.runtime().set_max_image_base64_bytes(cap);
    let description = runner
        .runtime()
        .tool_specs()
        .into_iter()
        .find(|spec| spec.name == "view_image")
        .unwrap()
        .description;
    assert!(
        description.contains(&format!("{cap} bytes")),
        "{description}"
    );
}

async fn call(runner: &SessionRunner, host: &str, name: &str, mut input: Value) -> ToolResult {
    input["host"] = host.into();
    runner
        .runtime()
        .clone()
        .dispatch(
            ToolUse {
                name: name.into(),
                input,
            },
            CancelToken::new(),
            CancelToken::new(),
        )
        .await
}

async fn scenario(host: &str, home: &Path) {
    let ceiling = 64;
    let config = HarnessConfig {
        max_image_base64_bytes: ceiling,
        remote_hosts: if host == "remote" {
            vec![HostConfig {
                name: host.into(),
                command: vec![
                    env!("CARGO_BIN_EXE_myco").into(),
                    "--mode".into(),
                    "host".into(),
                    "--max-image-base64-bytes".into(),
                    ceiling.to_string(),
                ],
            }]
        } else {
            vec![]
        },
        ..Default::default()
    };
    let harness = Harness::attach(config).await.unwrap();
    let mut small = runner(harness.clone(), 16).await;
    let mut large = runner(harness.clone(), 64).await;
    let image = home.join("image.png");
    std::fs::write(&image, [b"\x89PNG".as_slice(), &[0; 29]].concat()).unwrap(); // 44 base64 bytes
    let input = json!({"path":image});
    let (rejected, accepted) = tokio::join!(
        call(&small, host, "view_image", input.clone()),
        call(&large, host, "view_image", input.clone())
    );
    assert!(rejected.is_error, "{rejected:?}");
    assert!(test_utils::tool_text(&rejected).contains("limit"));
    assert!(!accepted.is_error, "{accepted:?}");

    let started = call(&small, host, "bash", json!({"action":"start","session_id":"keep","command":"bash","stdin":"counter=41\n","idle_ms":10,"timeout_ms":100})).await;
    assert!(!started.is_error, "{started:?}");
    let file = home.join("edit.txt");
    std::fs::write(&file, "before").unwrap();
    assert!(
        !call(
            &small,
            host,
            "str_replace_based_edit_tool",
            json!({"command":"view","path":file})
        )
        .await
        .is_error
    );

    let before = small.runtime().resources().await;
    select(&mut small, 64).await;
    select(&mut large, 16).await;
    let (accepted, rejected) = tokio::join!(
        call(&small, host, "view_image", input.clone()),
        call(&large, host, "view_image", input)
    );
    assert!(!accepted.is_error, "{accepted:?}");
    assert!(rejected.is_error, "{rejected:?}");
    assert_eq!(small.runtime().resources().await, before);
    let shell = call(&small, host, "bash", json!({"action":"write","session_id":"keep","stdin":"printf 'value=%s\\n' \"$((counter+1))\"\n","idle_ms":10,"timeout_ms":1000})).await;
    assert!(!shell.is_error, "{shell:?}");
    assert!(
        test_utils::tool_text(&shell).contains("value=42"),
        "{shell:?}"
    );
    let edited = call(
        &small,
        host,
        "str_replace_based_edit_tool",
        json!({"command":"str_replace","path":file,"old_str":"before","new_str":"after"}),
    )
    .await;
    assert!(!edited.is_error, "{edited:?}");
    assert_eq!(std::fs::read_to_string(file).unwrap(), "after");

    // A direct caller may omit the active cap, but cannot raise the worker ceiling.
    std::fs::write(&image, [b"\x89PNG".as_slice(), &[0; 45]].concat()).unwrap(); // 68 base64 bytes
    for cap in [None, Some(u64::MAX)] {
        let mut context = HostDispatchContext::new(uuid::Uuid::new_v4(), CancelToken::new());
        context.max_image_base64_bytes = cap;
        let result = harness
            .clone()
            .dispatch_tool_use_controlled(
                ToolUse {
                    name: "view_image".into(),
                    input: json!({"host":host,"path":image}),
                },
                context,
            )
            .await;
        assert!(result.is_error, "{result:?}");
    }
}

fn isolated(host: &str) {
    if let Some(home) = std::env::var_os("MYCO_IMAGE_LIMIT_FIXTURE") {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(scenario(host, Path::new(&home)));
        return;
    }
    let home =
        std::env::temp_dir().join(format!("myco-active-image-limits-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&home).unwrap();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            &format!("{host}_models_share_workers_and_keep_resources"),
            "--nocapture",
        ])
        .env("MYCO_HOME", &home)
        .env("MYCO_IMAGE_LIMIT_FIXTURE", &home)
        .output()
        .unwrap();
    std::fs::remove_dir_all(home).unwrap();
    assert!(
        result.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn local_models_share_workers_and_keep_resources() {
    isolated("local");
}

#[test]
fn remote_models_share_workers_and_keep_resources() {
    isolated("remote");
}

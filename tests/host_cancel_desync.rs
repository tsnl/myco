//! Regression: cancel / drop mid host call must not leave the NDJSON pipe
//! desynced so subsequent host calls hang or fail with correlation mismatch.
//!
//! Concurrent model: cancel only abandons that waiter's result. The host stays
//! up so sibling in-flight tools can still complete. Orphan replies are
//! discarded by the demux reader. Host death / I/O error still fails waiters
//! and clears the connection for lazy respawn.

mod test_utils;

use std::sync::Arc;
use std::time::Duration;

use myco::core::CancelToken;
use myco::generative_model::ToolUse;
use myco::harness::{HostConfig, HostController};
use serde_json::json;
use test_utils::tool_text;

fn subprocess_host() -> Arc<HostController> {
    let cap = myco::config::DEFAULT_MAX_IMAGE_BASE64_BYTES;
    HostController::new(
        HostConfig {
            name: "subprocess".into(),
            command: vec![
                env!("CARGO_BIN_EXE_myco").into(),
                "--mode".into(),
                "host".into(),
                "--name".into(),
                "subprocess".into(),
                "--max-image-base64-bytes".into(),
                cap.to_string(),
            ],
        },
        cap,
    )
}

fn process_with(command_fragment: &str) -> bool {
    let output = std::process::Command::new("ps")
        .args(["-ax", "-o", "command="])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.contains(command_fragment))
}

#[tokio::test]
async fn background_remote_exec_preserves_the_process_and_connection() {
    let client = subprocess_host();
    let owner = uuid::Uuid::new_v4();
    let context = myco::tool_services::HostDispatchContext::new(owner, CancelToken::new());
    // Request before dispatch to exercise a Background line arriving before the
    // host's tool task is polled. Registration must retain that request.
    context.background.cancel();
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        client.call_controlled(
            ToolUse {
                name: "bash".into(),
                input: json!({"command":"echo before-background; sleep 30", "timeout_ms":60000}),
            },
            context.clone(),
        ),
    )
    .await
    .expect("background request did not release the remote wait");
    assert_eq!(result.status.as_deref(), Some("backgrounded"), "{result:?}");
    let resources = client.resources(owner).await.unwrap();
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0].details["process_exited"], false);
    let id = &resources[0].id;
    context.cancel.cancel();
    let read = client
        .call(
            owner,
            ToolUse {
                name: "bash".into(),
                input: json!({"action":"read", "session_id":id, "timeout_ms":1000}),
            },
            CancelToken::new(),
        )
        .await;
    assert!(!read.is_error, "{read:?}");
    assert!(format!("{}{}", tool_text(&result), tool_text(&read)).contains("before-background"));
    let next = client
        .call(
            owner,
            ToolUse {
                name: "bash".into(),
                input: json!({"command":"echo foreground-ready"}),
            },
            CancelToken::new(),
        )
        .await;
    assert!(tool_text(&next).contains("foreground-ready"), "{next:?}");
    client
        .call(
            owner,
            ToolUse {
                name: "bash".into(),
                input: json!({"action":"close", "session_id":id}),
            },
            CancelToken::new(),
        )
        .await;
    assert!(client.resources(owner).await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancel_midcall_then_next_call_succeeds() {
    let client = subprocess_host();
    let sleep_tag = format!("97.{}", uuid::Uuid::new_v4().as_u128() % 100_000);

    let cancel = CancelToken::new();
    // Same-task delayed cancel (avoids spawn scheduling races under suite load).
    let mut call = std::pin::pin!(client.call(
        uuid::Uuid::nil(),
        ToolUse {
            name: "bash".into(),
            input: json!({
                "command": format!("sleep {sleep_tag}; echo done-slow"),
                "timeout_ms": 180_000
            }),
        },
        cancel.clone(),
    ));
    let cancelled = tokio::select! {
        r = &mut call => r,
        _ = tokio::time::sleep(Duration::from_millis(400)) => {
            cancel.cancel();
            call.await
        }
    };
    assert!(cancelled.is_error, "{cancelled:?}");
    assert!(tool_text(&cancelled).contains("cancelled"), "{cancelled:?}");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while process_with(&format!("sleep {sleep_tag}")) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "cancelled remote command survived on the host"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Next call must complete on the live (or respawned) connection.
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        client.call(
            uuid::Uuid::nil(),
            ToolUse {
                name: "bash".into(),
                input: json!({"command": "echo hello-after-cancel"}),
            },
            CancelToken::new(),
        ),
    )
    .await
    .expect("next call timed out");

    assert!(!result.is_error, "next call errored: {result:?}");
    assert!(
        tool_text(&result).contains("hello-after-cancel"),
        "expected reply after cancel, got: {:?}",
        tool_text(&result)
    );

    let again = client
        .call(
            uuid::Uuid::nil(),
            ToolUse {
                name: "bash".into(),
                input: json!({"command": "echo second-ok"}),
            },
            CancelToken::new(),
        )
        .await;
    assert!(!again.is_error, "{again:?}");
    assert!(tool_text(&again).contains("second-ok"), "{again:?}");
}

#[tokio::test]
async fn drop_midcall_then_next_call_succeeds() {
    let client = subprocess_host();

    // Simulate agent tokio::select! dropping the call future on Ctrl-C.
    let slow = client.call(
        uuid::Uuid::nil(),
        ToolUse {
            name: "bash".into(),
            input: json!({"command": "sleep 2; echo done-slow"}),
        },
        CancelToken::new(),
    );
    tokio::select! {
        _ = slow => panic!("slow call finished before drop"),
        _ = tokio::time::sleep(Duration::from_millis(200)) => {}
    }

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        client.call(
            uuid::Uuid::nil(),
            ToolUse {
                name: "bash".into(),
                input: json!({"command": "echo after-drop"}),
            },
            CancelToken::new(),
        ),
    )
    .await
    .expect("next call timed out");

    assert!(!result.is_error, "next call after drop: {result:?}");
    assert!(
        tool_text(&result).contains("after-drop"),
        "{:?}",
        tool_text(&result)
    );
}

#[tokio::test]
async fn remote_inventory_preserves_lazy_connections_and_partitions_resources_by_owner() {
    let client = subprocess_host();
    let owner = uuid::Uuid::new_v4();
    assert!(
        client
            .resources(owner)
            .await
            .unwrap_err()
            .contains("not connected")
    );
    assert!(!client.is_connected());
    let result = client.call(owner, ToolUse { name: "bash".into(), input: json!({
        "action":"start", "session_id":"retained", "command":"cat", "idle_ms":10, "timeout_ms":1000,
    }) }, CancelToken::new()).await;
    assert!(!result.is_error, "{result:?}");
    let resources = client.resources(owner).await.unwrap();
    assert_eq!(resources.len(), 1);
    assert_eq!(resources[0].id, "retained");
    assert_eq!(resources[0].details["process_exited"], false);
    assert!(
        client
            .resources(uuid::Uuid::new_v4())
            .await
            .unwrap()
            .is_empty()
    );
    client
        .call(
            owner,
            ToolUse {
                name: "bash".into(),
                input: json!({"action":"close", "session_id":"retained"}),
            },
            CancelToken::new(),
        )
        .await;
    assert!(client.resources(owner).await.unwrap().is_empty());
}

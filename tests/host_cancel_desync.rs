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

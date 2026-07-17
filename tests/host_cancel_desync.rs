//! Regression: cancel / drop mid host call must not leave the NDJSON pipe
//! desynced so subsequent host calls hang or fail with correlation mismatch.
//!
//! Concurrent model: cancel only abandons that waiter's result. The host stays
//! up so sibling in-flight tools can still complete. Orphan replies are
//! discarded by the demux reader. Host death / I/O error still fails waiters
//! and clears the connection for lazy respawn.

use std::time::Duration;

use myco::core::CancelToken;
use myco::generative_model::{Content, ToolUse};
use myco::harness::HostController;
use serde_json::json;

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

#[tokio::test]
async fn cancel_midcall_then_next_call_succeeds() {
    let client = HostController::local_in_process();

    let cancel = CancelToken::new();
    let cancel_bg = cancel.clone();
    tokio::spawn(async move {
        // Cancel after the bash child is almost certainly running.
        tokio::time::sleep(Duration::from_millis(300)).await;
        cancel_bg.cancel();
    });

    let cancelled = client
        .call(
            uuid::Uuid::nil(),
            ToolUse {
                id: "slow".into(),
                name: "bash".into(),
                // Long enough that cancel always races before natural exit.
                input: json!({"command": "sleep 30; echo done-slow"}),
            },
            cancel,
        )
        .await;
    assert!(cancelled.is_error, "{cancelled:?}");
    assert!(tool_text(&cancelled).contains("cancelled"), "{cancelled:?}");

    // Next call must complete on the live (or respawned) connection.
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        client.call(
            uuid::Uuid::nil(),
            ToolUse {
                id: "next".into(),
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
                id: "again".into(),
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
    let client = HostController::local_in_process();

    // Simulate agent tokio::select! dropping the call future on Ctrl-C.
    let slow = client.call(
        uuid::Uuid::nil(),
        ToolUse {
            id: "slow".into(),
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
                id: "next".into(),
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

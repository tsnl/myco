//! The whole providers bet, with real processes: a `host` instance dials
//! this crate's own binary over piped stdio — exactly the shape `ssh box
//! myco-hostd` has — a tty is created over there through the host's `new`
//! verb, and from then on the near pool cannot tell it is remote: verbs,
//! watermarks, and text all flow through the ordinary surface.

use std::sync::Arc;

use myco_instance::{Pool, Principal};
use serde_json::{Value, json};

fn ada() -> Principal {
    Principal::Human("ada".into())
}

fn in_secs(s: u64) -> tokio::time::Instant {
    tokio::time::Instant::now() + std::time::Duration::from_secs(s)
}

#[tokio::test]
async fn a_host_dials_the_real_hostd_and_a_far_tty_answers() {
    let pool = Pool::new();
    pool.register(Arc::new(myco_provider::HostKind::new(pool.clone())));

    let command = format!("{} --name farbox", env!("CARGO_BIN_EXE_myco-hostd"));
    let host = pool
        .create(&ada(), "host", "", "far-box", json!({ "command": command }))
        .expect("the host instance creates");

    // The dial is a side-feed; status is state, so the wait is the
    // ordinary one: read, check, wake on the watermark.
    let up = pool
        .wait_until(&ada(), &host.id, "about", in_secs(30), |about| {
            about["status"] == "up"
        })
        .await
        .expect("about answers")
        .expect("the host came up before the deadline");
    assert_eq!(up["name"], "farbox");
    assert_eq!(up["kinds"][0]["kind"], "tty");

    // Create over there through the host's own vocabulary — no sys.new
    // change, no L2 involvement.
    let row = pool
        .call(
            &ada(),
            &host.id,
            "new",
            json!({
                "kind": "tty",
                "title": "far-echo",
                "args": { "mode": "piped", "command": "echo hello-from-far" },
            }),
        )
        .await
        .expect("creates a tty over there");
    let tty = row["id"].as_str().expect("a row id").to_string();

    // Adoption is asynchronous (the row frame follows the reply); once
    // listed, the row nests under the host.
    let deadline = in_secs(10);
    loop {
        if let Some(listed) = pool.list(None).into_iter().find(|r| r.id == tty) {
            assert_eq!(listed.parent.as_deref(), Some(host.id.as_str()));
            assert_eq!(listed.kind, "tty");
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the tty row was adopted in time"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    // The far command's output arrives through the ordinary read, woken
    // by relayed marks — the caller cannot tell where the pty lives.
    let text = pool
        .wait_until(&ada(), &tty, "text", in_secs(30), |answer| {
            answer["text"]
                .as_str()
                .is_some_and(|t| t.contains("hello-from-far"))
        })
        .await
        .expect("text answers")
        .expect("the far output landed before the deadline");
    assert_eq!(text["exit_code"], 0, "the far command finished cleanly");

    // Removal is the same story told backwards.
    pool.call(&ada(), &tty, "sys.remove", Value::Null)
        .await
        .expect("removes over the wire");
    let deadline = in_secs(10);
    while pool.list(None).iter().any(|r| r.id == tty) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the tty row was dropped in time"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

use super::*;
use crate::test_support::{result_text, temp_dir};
use serde_json::json;

async fn call(
    service: &Arc<BashService>,
    owner: Uuid,
    input: serde_json::Value,
) -> generative_model::ToolResult {
    service
        .clone()
        .dispatch_tool_use(
            generative_model::ToolUse {
                name: "bash".into(),
                input,
            },
            HostDispatchContext::new(owner, crate::core::CancelToken::new()),
        )
        .await
}

async fn wait_for(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("process did not reach the expected state");
}

#[tokio::test]
async fn timed_background_preserves_pid_output_and_owner_without_restarting() {
    let service = Arc::new(BashService::new());
    let owner = Uuid::new_v4();
    let dir = temp_dir("background-after-continuity");
    let result = call(&service, owner, json!({
        "command":format!("echo $$ > '{0}/pid'; echo before; while [ ! -f '{0}/release' ]; do sleep 0.01; done; echo after", dir.path().display()),
        "background_after":50,
    })).await;
    assert_eq!(result.status.as_deref(), Some("backgrounded"));
    let id = result.resource.as_ref().unwrap().id.clone();
    wait_for(|| dir.path().join("pid").exists()).await;
    let pid: u32 = std::fs::read_to_string(dir.path().join("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(service.resources(owner)[0].details["pid"], pid);
    assert!(result_text(&result).contains("background_after=50ms"));
    assert!(!result_text(&result).contains("User backgrounded"));
    let foreign = call(
        &service,
        Uuid::new_v4(),
        json!({"action":"close", "session_id":id}),
    )
    .await;
    assert!(foreign.is_error);
    std::fs::write(dir.path().join("release"), "").unwrap();
    let shared = service.sessions()[&id].shared.clone();
    wait_for(|| lock_unpoisoned(&shared.buffer).is_finished()).await;
    let read = call(
        &service,
        owner,
        json!({"action":"read", "session_id":id, "timeout_ms":1000}),
    )
    .await;
    let output = format!("{}{}", result_text(&result), result_text(&read));
    assert_eq!(output.lines().filter(|line| *line == "before").count(), 1);
    assert_eq!(output.lines().filter(|line| *line == "after").count(), 1);
    assert_eq!(read.resource, result.resource);
    assert_eq!(read.status.as_deref(), Some("exit 0"));
    assert!(
        !call(&service, owner, json!({"action":"close", "session_id":id}))
            .await
            .is_error
    );
}

#[tokio::test]
async fn a_command_that_finishes_before_background_after_returns_its_exit() {
    let service = Arc::new(BashService::new());
    let owner = Uuid::new_v4();
    let result = call(
        &service,
        owner,
        json!({"command":"echo completed; exit 7", "background_after":2000}),
    )
    .await;
    assert_eq!(result.status.as_deref(), Some("exit 7"));
    assert!(result_text(&result).contains("completed"));
    assert!(result.resource.is_none());
    assert!(service.resources(owner).is_empty());
}

#[tokio::test]
async fn an_explicit_deadline_still_kills_the_same_group_after_timed_yield() {
    let service = Arc::new(BashService::new());
    let owner = Uuid::new_v4();
    let result = call(
        &service,
        owner,
        json!({"command":"echo before; sleep 30", "background_after":20, "timeout_ms":200}),
    )
    .await;
    assert_eq!(result.status.as_deref(), Some("backgrounded"));
    let id = result.resource.as_ref().unwrap().id.clone();
    assert!(result_text(&result).contains("explicit timeout_ms=200 remains"));
    let shared = service.sessions()[&id].shared.clone();
    wait_for(|| lock_unpoisoned(&shared.buffer).is_finished()).await;
    assert_eq!(service.resources(owner)[0].details["exec_timeout_ms"], 200);
    let read = call(&service, owner, json!({"action":"read", "session_id":id})).await;
    assert_eq!(
        read.status.as_deref(),
        Some("timed out after 200ms; process group killed")
    );
    assert!(result_text(&read).contains("exit_signal: Some(9)"));
    assert!(result_text(&read).contains("explicit timeout_ms=200"));
    call(&service, owner, json!({"action":"close", "session_id":id})).await;
}

#[tokio::test(start_paused = true)]
async fn omitted_timeout_never_kills_a_timed_exec_before_or_after_promotion() {
    let service = Arc::new(BashService::new());
    let owner = Uuid::new_v4();
    let result = call(
        &service,
        owner,
        json!({"command":"sleep 30", "background_after":90000}),
    )
    .await;
    assert_eq!(result.status.as_deref(), Some("backgrounded"));
    let id = result.resource.as_ref().unwrap().id.clone();
    tokio::time::advance(Duration::from_millis(DEFAULT_EXEC_TIMEOUT_MS + 1)).await;
    assert_eq!(service.resources(owner)[0].details["process_exited"], false);
    assert_eq!(
        service.resources(owner)[0].details["exec_timeout_ms"],
        serde_json::Value::Null
    );
    call(&service, owner, json!({"action":"close", "session_id":id})).await;
}

#[tokio::test]
async fn a_deadline_before_or_equal_to_background_after_wins_without_a_handle() {
    let service = Arc::new(BashService::new());
    let owner = Uuid::new_v4();
    for timeout in [10, 20] {
        let result = call(
            &service,
            owner,
            json!({"command":"sleep 30", "background_after":20, "timeout_ms":timeout}),
        )
        .await;
        assert!(
            result.status.as_deref().unwrap().starts_with("timed out"),
            "{result:?}"
        );
        assert!(result.resource.is_none());
        assert!(service.resources(owner).is_empty());
    }
}

#[tokio::test]
async fn cancellation_before_timed_yield_kills_without_retaining_a_handle() {
    let service = Arc::new(BashService::new());
    let owner = Uuid::new_v4();
    let context = HostDispatchContext::new(owner, crate::core::CancelToken::new());
    let dir = temp_dir("background-after-cancel");
    let work = service.clone().dispatch_tool_use(
        generative_model::ToolUse {
            name: "bash".into(),
            input: json!({
                "command":format!("echo $$ > '{}/pid'; sleep 30", dir.path().display()),
                "background_after":5000,
            }),
        },
        context.clone(),
    );
    let (result, ()) = tokio::join!(work, async {
        wait_for(|| dir.path().join("pid").exists()).await;
        context.cancel.cancel();
    });
    assert!(result.is_error);
    assert!(result_text(&result).contains("exec cancelled"));
    assert!(result.resource.is_none());
    assert!(service.resources(owner).is_empty());
    let pid: u32 = std::fs::read_to_string(dir.path().join("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
}

#[tokio::test]
async fn the_deadline_task_does_not_keep_a_closed_process_owner_alive() {
    let service = Arc::new(BashService::new());
    let owner = Uuid::new_v4();
    let result = call(
        &service,
        owner,
        json!({"command":"sleep 30", "background_after":10, "timeout_ms":30000}),
    )
    .await;
    let id = result.resource.as_ref().unwrap().id.clone();
    let weak = Arc::downgrade(&service.sessions()[&id].process);
    call(&service, owner, json!({"action":"close", "session_id":id})).await;
    assert!(weak.upgrade().is_none());
    assert!(service.resources(owner).is_empty());
}

#[tokio::test]
async fn invalid_background_after_is_rejected_before_starting_commands() {
    let service = Arc::new(BashService::new());
    let owner = Uuid::new_v4();
    let dir = temp_dir("background-after-invalid");
    for value in [
        json!(0),
        json!(MAX_EXEC_TIMEOUT_MS + 1),
        json!(-1),
        json!("1s"),
    ] {
        let result = call(
            &service,
            owner,
            json!({
                "command":format!("touch '{}/ran'", dir.path().display()), "background_after":value,
            }),
        )
        .await;
        assert!(result.is_error, "{result:?}");
        if value.is_u64() {
            assert!(result_text(&result).contains("background_after"));
        }
    }
    let invalid = call(&service, owner, json!({"action":"start", "session_id":"invalid", "command":"sleep 30", "background_after":1})).await;
    assert!(invalid.is_error);
    assert!(!dir.path().join("ran").exists());
    assert!(service.resources(owner).is_empty());
}

#[test]
fn exec_wait_policy_has_no_implicit_deadline_when_background_after_is_set() {
    for (input, expected) in [
        (json!({"command":"true"}), Some(DEFAULT_EXEC_TIMEOUT_MS)),
        (json!({"command":"true", "background_after":90000}), None),
        (
            json!({"command":"true", "background_after":90000, "timeout_ms":120000}),
            Some(120000),
        ),
    ] {
        let input: Input = serde_json::from_value(input).unwrap();
        let Action::Exec { timeout_ms, .. } = resolve_action(&input).unwrap() else {
            panic!("exec")
        };
        assert_eq!(timeout_ms, expected);
    }
}

#[tokio::test]
async fn explicit_deadline_reaps_redirected_descendants_after_the_leader_finishes() {
    let service = Arc::new(BashService::new());
    let owner = Uuid::new_v4();
    let directory = temp_dir("background-after-descendants");
    let result = call(&service, owner, json!({
        "command": format!("while [ ! -f '{0}/release' ]; do sleep 0.01; done; sleep 30 >/dev/null 2>&1 & echo $! > '{0}/pid'", directory.path().display()),
        "background_after":10, "timeout_ms":1000,
    })).await;
    assert_eq!(result.status.as_deref(), Some("backgrounded"));
    let id = result.resource.as_ref().unwrap().id.clone();
    let shared = service.sessions()[&id].shared.clone();
    std::fs::write(directory.path().join("release"), "").unwrap();
    wait_for(|| lock_unpoisoned(&shared.buffer).is_finished()).await;
    let pid: u32 = std::fs::read_to_string(directory.path().join("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(super::tests::process_is_running(pid));
    wait_for(|| !super::tests::process_is_running(pid)).await;
    let read = call(&service, owner, json!({"action":"read", "session_id":id})).await;
    assert_eq!(read.status.as_deref(), Some("exit 0"));
    assert_eq!(
        service.resources(owner)[0].details["exec_timeout_ms"],
        serde_json::Value::Null
    );
    call(&service, owner, json!({"action":"close", "session_id":id})).await;
}

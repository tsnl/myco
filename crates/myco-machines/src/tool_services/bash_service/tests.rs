use super::*;
use crate::host::HostWorker;
use myco_test_support::{result_text, temp_dir};
use serde_json::json;

fn tool_use_json(value: serde_json::Value) -> myco_api::ToolUse {
    myco_api::ToolUse {
        id: "test".into(),
        name: "bash".into(),
        input: value,
    }
}

fn harness() -> Arc<HostWorker> {
    Arc::new(HostWorker::new(
        "test",
        vec![Arc::new(BashService::new()) as Arc<dyn ToolService>],
    ))
}

fn dispatch_ctx(agent_id: uuid::Uuid) -> HostDispatchContext {
    HostDispatchContext::new(agent_id, myco_core::CancelToken::new())
}

async fn dispatch_json(harness: Arc<HostWorker>, value: serde_json::Value) -> myco_api::ToolResult {
    harness
        .dispatch_tool_use(tool_use_json(value), dispatch_ctx(uuid::Uuid::nil()))
        .await
}

/// Owner-scoped dispatch straight at the service, for ownership / summary
/// tests that need a specific `agent_id`.
async fn dispatch_json_as(
    service: &Arc<BashService>,
    owner: uuid::Uuid,
    value: serde_json::Value,
) -> myco_api::ToolResult {
    service
        .clone()
        .dispatch_tool_use(tool_use_json(value), dispatch_ctx(owner))
        .await
}

fn unique_id(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4().as_simple())
}

// Session-lifecycle helpers with default fast test timings. Tests where the
// timings or extra fields ARE the claim build their own json via
// `dispatch_json` instead.

/// `start` a session (timeout 1s / idle 200ms), assert success, return the
/// result text.
async fn start_ok(harness: &Arc<HostWorker>, id: &str, command: &str) -> String {
    let result = dispatch_json(
        harness.clone(),
        json!({
            "action": "start",
            "session_id": id,
            "command": command,
            "timeout_ms": 1000,
            "idle_ms": 200,
        }),
    )
    .await;
    assert!(!result.is_error, "start: {}", result_text(&result));
    result_text(&result)
}

/// `write` stdin into a session (timeout 1s / idle 300ms), assert success,
/// return the result text.
async fn write_ok(harness: &Arc<HostWorker>, id: &str, stdin: &str) -> String {
    let result = dispatch_json(
        harness.clone(),
        json!({
            "action": "write",
            "session_id": id,
            "stdin": stdin,
            "timeout_ms": 1000,
            "idle_ms": 300,
        }),
    )
    .await;
    assert!(!result.is_error, "write: {}", result_text(&result));
    result_text(&result)
}

/// `close` a session, returning the raw result (cleanup callers ignore it).
async fn close(harness: &Arc<HostWorker>, id: &str) -> myco_api::ToolResult {
    dispatch_json(
        harness.clone(),
        json!({"action": "close", "session_id": id}),
    )
    .await
}

#[test]
fn bare_command_resolves_to_exec() {
    let input: Input = serde_json::from_value(json!({"command": "echo hi"})).unwrap();
    let action = resolve_action(&input).unwrap();
    match action {
        Action::Exec {
            command,
            cwd,
            timeout_ms,
            max_bytes,
        } => {
            assert_eq!(command, "echo hi");
            assert_eq!(cwd, None);
            assert_eq!(timeout_ms, DEFAULT_EXEC_TIMEOUT_MS);
            assert_eq!(max_bytes, DEFAULT_MAX_BYTES);
        }
        _ => panic!("expected Exec"),
    }
}

#[test]
fn rejects_empty_input() {
    let input: Input = serde_json::from_value(json!({})).unwrap();
    let err = resolve_action(&input).unwrap_err();
    assert!(err.contains("empty bash input"), "err={err}");
    // The message has to be actionable: name the fields that would fix it.
    assert!(
        err.contains("command") && err.contains("action"),
        "err={err}"
    );
}

/// `{"input": {}}` is a real thing models emit, and explicit nulls are the
/// same nothing. Both must come back as an error tool result the agent can
/// recover from — never a dropped or empty success.
#[tokio::test]
async fn empty_input_dispatch_returns_error_result() {
    for value in [
        json!({}),
        json!({"action": null, "command": null, "session_id": null}),
    ] {
        let result = dispatch_json(harness(), value.clone()).await;
        assert!(result.is_error, "value={value} expected error: {result:?}");
        assert_eq!(result.id, "test");
        let text = result_text(&result);
        assert!(
            text.contains("empty bash input"),
            "value={value} text={text}"
        );
    }
}

/// Non-object inputs (`null`, a bare string, a list) are malformed too.
#[tokio::test]
async fn non_object_input_dispatch_returns_error_result() {
    for value in [json!(null), json!("ls"), json!([]), json!(7)] {
        let result = dispatch_json(harness(), value.clone()).await;
        assert!(result.is_error, "value={value} result={result:?}");
        let text = result_text(&result);
        assert!(
            text.contains("must be a JSON object"),
            "value={value} text={text}"
        );
    }
}

/// A blank `command` would run `bash -c ""`: exit 0, no output, nothing done.
#[test]
fn rejects_blank_command() {
    for command in ["", "   ", "\n\t"] {
        let input: Input = serde_json::from_value(json!({"command": command})).unwrap();
        let err = resolve_action(&input).unwrap_err();
        assert!(err.contains("`command` must be a non-empty"), "err={err}");

        let input: Input = serde_json::from_value(
            json!({"action": "start", "session_id": "s", "command": command}),
        )
        .unwrap();
        let err = resolve_action(&input).unwrap_err();
        assert!(err.contains("`command` must be a non-empty"), "err={err}");
    }
}

#[test]
fn rejects_blank_session_id() {
    for action in ["start", "write", "read", "signal", "close"] {
        let input: Input = serde_json::from_value(
            json!({"action": action, "session_id": "  ", "stdin": "x", "command": "true"}),
        )
        .unwrap();
        let err = resolve_action(&input).unwrap_err();
        assert!(
            err.contains("`session_id` must be a non-empty"),
            "action={action} err={err}"
        );
    }
}

/// The tool description is the model-facing contract: it must state the
/// defaults/limits actually enforced, not stale hardcoded copies.
#[test]
fn tool_description_states_actual_defaults() {
    let specs = BashService::new().tool_specs();
    let d = &specs[0].description;
    for needle in [
        DEFAULT_EXEC_TIMEOUT_MS.to_string(),
        MAX_EXEC_TIMEOUT_MS.to_string(),
        DEFAULT_TIMEOUT_MS.to_string(),
        MAX_TIMEOUT_MS.to_string(),
        DEFAULT_IDLE_MS.to_string(),
        DEFAULT_MAX_BYTES.to_string(),
    ] {
        assert!(d.contains(&needle), "description missing {needle}: {d}");
    }
}

#[test]
fn rejects_command_starting_with_cd() {
    for command in [
        "cd /tmp && ls",
        "  cd /tmp",
        "cd\t/tmp",
        "cd",
        "cd /tmp; ls",
    ] {
        let input: Input = serde_json::from_value(json!({"command": command})).unwrap();
        let err = resolve_action(&input).unwrap_err();
        assert!(
            err.contains("must not start with `cd`") && err.contains("`cwd`"),
            "command={command:?} err={err}"
        );
    }

    // Not a leading shell `cd` word — allowed.
    for command in ["cdo something", "echo cd /tmp", "true && cd /tmp"] {
        let input: Input = serde_json::from_value(json!({"command": command})).unwrap();
        assert!(
            resolve_action(&input).is_ok(),
            "should allow command={command:?}"
        );
    }
}

#[test]
fn rejects_cwd_on_non_spawn_actions() {
    for action in ["write", "read", "signal", "close", "list"] {
        let input: Input = serde_json::from_value(json!({
            "action": action,
            "cwd": "/tmp",
        }))
        .unwrap();
        let err = resolve_action(&input).unwrap_err();
        assert!(
            err.contains("`cwd` is only valid"),
            "action={action} err={err}"
        );
    }
}

#[test]
fn cwd_resolves_on_exec_and_start() {
    let input: Input = serde_json::from_value(json!({
        "command": "pwd",
        "cwd": " /tmp ",
    }))
    .unwrap();
    match resolve_action(&input).unwrap() {
        Action::Exec { cwd, .. } => assert_eq!(cwd.as_deref(), Some("/tmp")),
        _ => panic!("expected Exec"),
    }

    let input: Input = serde_json::from_value(json!({
        "action": "start",
        "session_id": "s",
        "command": "bash --noprofile --norc",
        "cwd": "/var",
    }))
    .unwrap();
    match resolve_action(&input).unwrap() {
        Action::Start { cwd, .. } => assert_eq!(cwd.as_deref(), Some("/var")),
        _ => panic!("expected Start"),
    }
}

/// Shared timeout-resolution contract for one action shape: the default
/// applies when omitted, an explicit under-ceiling value is preserved, and a
/// value above the safety ceiling is rejected (not clamped).
fn assert_timeout_resolution(base: serde_json::Value, default_ms: u64, max_ms: u64) {
    let resolve = |timeout_ms: Option<u64>| {
        let mut value = base.clone();
        if let Some(t) = timeout_ms {
            value["timeout_ms"] = t.into();
        }
        let input: Input = serde_json::from_value(value).unwrap();
        resolve_action(&input)
    };
    let timeout_of = |action: Action| match action {
        Action::Exec { timeout_ms, .. } | Action::Read { timeout_ms, .. } => timeout_ms,
        other => panic!("expected Exec/Read, got {other:?}"),
    };

    // Default when omitted.
    assert_eq!(timeout_of(resolve(None).unwrap()), default_ms);
    // Explicit multi-minute value under the ceiling is preserved.
    assert_eq!(timeout_of(resolve(Some(120_000)).unwrap()), 120_000);
    // Above the safety ceiling is rejected (not clamped).
    let err = resolve(Some(max_ms + 1)).unwrap_err();
    assert!(
        err.contains("exceeds max") && err.contains(&max_ms.to_string()),
        "{err}"
    );
}

#[test]
fn timeout_ms_defaults_and_rejects_above_session_max() {
    assert_eq!(DEFAULT_TIMEOUT_MS, 30_000);
    assert_eq!(MAX_TIMEOUT_MS, 1_800_000);
    assert_timeout_resolution(
        json!({"action": "read", "session_id": "s"}),
        DEFAULT_TIMEOUT_MS,
        MAX_TIMEOUT_MS,
    );
}

#[test]
fn exec_timeout_ms_defaults_to_60s_and_rejects_above_max() {
    assert_eq!(DEFAULT_EXEC_TIMEOUT_MS, 60_000);
    assert_eq!(MAX_EXEC_TIMEOUT_MS, 1_800_000);
    assert_timeout_resolution(
        json!({"action": "exec", "command": "true"}),
        DEFAULT_EXEC_TIMEOUT_MS,
        MAX_EXEC_TIMEOUT_MS,
    );
}

#[test]
fn truncate_middle_keeps_head_and_tail() {
    // At or under the cap: untouched, no marker.
    assert_eq!(truncate_middle_lossy(b"hello", 5), "hello");
    assert_eq!(truncate_middle_lossy(b"", 5), "");

    // Over the cap: head + marker + tail, omitted count exact.
    let bytes: Vec<u8> = (0..26).map(|i| b'a' + i).collect();
    let text = truncate_middle_lossy(&bytes, 10);
    assert!(text.starts_with("abcde"), "{text}");
    assert!(text.ends_with("vwxyz"), "{text}");
    assert!(text.contains("16 bytes omitted"), "{text}");
    assert!(text.contains("26 bytes total"), "{text}");
    assert!(text.contains("max_bytes=10"), "{text}");

    // Odd cap: head gets the extra byte; total kept is still the cap.
    let text = truncate_middle_lossy(&bytes, 5);
    assert!(text.starts_with("abc"), "{text}");
    assert!(text.ends_with("yz"), "{text}");
    assert!(text.contains("21 bytes omitted"), "{text}");
}

/// The capture buffer itself is capped: a stream bigger than
/// EXEC_CAPTURE_CAP keeps only head + rolling tail in memory, and the
/// rendered result folds capture loss and the return cap into one
/// accurate marker.
#[test]
fn capped_capture_bounds_memory_and_reports_total() {
    let mut cap = CappedCapture::default();
    let total: usize = EXEC_CAPTURE_CAP * 3;
    let chunk = vec![b'x'; 4096];
    let mut written = 0usize;
    while written < total {
        cap.push(&chunk);
        written += chunk.len();
    }
    assert!(cap.omitted > 0);
    assert!(
        cap.head.len() + cap.tail.len() <= EXEC_CAPTURE_CAP + 64 * 1024,
        "in-memory bytes must stay near the cap: {}",
        cap.head.len() + cap.tail.len()
    );
    assert_eq!(cap.head.len() + cap.omitted + cap.tail.len(), written);

    let text = render_capture(&cap, 1000);
    assert!(text.contains(&format!("{written} bytes total")), "{text}");
    assert!(text.contains("bytes omitted"), "{text}");
    // Rendered size respects the return cap (plus the marker line).
    assert!(text.len() < 1000 + 300, "rendered too big: {}", text.len());
}

/// A session stream over the in-memory cap drops oldest bytes in blocks
/// and stays bounded.
#[test]
fn session_stream_cap_drops_oldest() {
    let mut v = vec![0u8; SESSION_STREAM_CAP + 100];
    let dropped = cap_session_stream(&mut v);
    assert!(dropped >= 100, "{dropped}");
    assert!(v.len() <= SESSION_STREAM_CAP);
    // Under the cap: untouched.
    let mut small = vec![0u8; 1024];
    assert_eq!(cap_session_stream(&mut small), 0);
    assert_eq!(small.len(), 1024);
}

/// A flooding exec must come back capped — head and tail survive, the
/// middle is elided — instead of saturating the model context.
#[tokio::test]
async fn exec_truncates_flood_keeping_head_and_tail() {
    let harness = harness();
    let result = dispatch_json(
        harness,
        json!({
            "action": "exec",
            "command": "seq 1 100000",
            "max_bytes": 2000,
        }),
    )
    .await;
    assert!(!result.is_error, "{}", result_text(&result));
    let text = result_text(&result);
    assert!(text.contains("Exit code: Some(0)"), "{text}");
    // First lines survive (root-cause errors live at the head)…
    assert!(text.contains("stdout:\n1\n2\n"), "{text}");
    // …last lines survive (summaries live at the tail)…
    assert!(text.contains("100000"), "{text}");
    // …and the middle is elided with an honest marker.
    assert!(text.contains("bytes omitted"), "{text}");
    assert!(
        text.len() < 4_000,
        "result should be near the 2000-byte cap, got {} bytes: {text}",
        text.len()
    );
}

/// The default cap applies when `max_bytes` is omitted: unbounded floods
/// must not pass through whole.
#[tokio::test]
async fn exec_default_max_bytes_caps_output() {
    let harness = harness();
    let result = dispatch_json(
        harness,
        json!({
            "action": "exec",
            // ~1.2 MB of stdout, far over the 4 KiB default.
            "command": "seq 1 200000",
        }),
    )
    .await;
    assert!(!result.is_error, "{}", result_text(&result));
    let text = result_text(&result);
    assert!(text.contains("bytes omitted"), "{text}");
    assert!(
        text.len() < DEFAULT_MAX_BYTES + 2_048,
        "result should be near the {DEFAULT_MAX_BYTES}-byte default cap, got {} bytes",
        text.len()
    );
}

/// stdout and stderr are capped independently: a flooding stdout must not
/// starve stderr diagnostics, and both keep their ends.
#[tokio::test]
async fn exec_truncates_streams_independently() {
    let harness = harness();
    let result = dispatch_json(
        harness,
        json!({
            "action": "exec",
            "command": "seq 1 50000; { echo ERR-HEAD; seq 1 50000; echo ERR-TAIL; } 1>&2",
            "max_bytes": 1000,
        }),
    )
    .await;
    assert!(!result.is_error, "{}", result_text(&result));
    let text = result_text(&result);
    assert_eq!(
        text.matches("bytes omitted").count(),
        2,
        "both streams should be truncated: {text}"
    );
    assert!(text.contains("ERR-HEAD"), "{text}");
    assert!(text.contains("ERR-TAIL"), "{text}");
}

/// A runaway that floods then hangs: the timeout path must return capped
/// output too — the flood is exactly when the cap matters most.
#[tokio::test]
async fn exec_timeout_output_is_truncated() {
    let harness = harness();
    let result = dispatch_json(
        harness,
        json!({
            "action": "exec",
            "command": "seq 1 100000; sleep 30",
            "timeout_ms": 2_000,
            "max_bytes": 1000,
        }),
    )
    .await;
    assert!(!result.is_error, "{}", result_text(&result));
    let text = result_text(&result);
    assert!(text.contains("timed_out"), "{text}");
    assert!(text.contains("bytes omitted"), "{text}");
    assert!(
        text.len() < 3_000,
        "timed-out result should be capped, got {} bytes: {text}",
        text.len()
    );
}

/// Silent long-lived child: tool must return quickly with timed_out while
/// the process stays alive in the background for later read/close — and the
/// late `echo` must not appear in the returned stdout body.
#[tokio::test]
async fn session_returns_while_process_still_running() {
    let harness = harness();
    let id = unique_id("bg");

    let t0 = Instant::now();
    let start = dispatch_json(
        harness.clone(),
        json!({
            "action": "start",
            "session_id": id,
            // No output for 30s — must not block the tool call that long.
            "command": "bash -c 'sleep 30; echo late'",
            "timeout_ms": 1_000,
            "idle_ms": 200,
        }),
    )
    .await;
    let elapsed = t0.elapsed();
    assert!(!start.is_error, "start: {}", result_text(&start));
    assert!(
        elapsed < Duration::from_secs(3),
        "start should return in ~1s (session max), took {elapsed:?}: {}",
        result_text(&start)
    );
    let text = result_text(&start);
    assert!(
        text.contains("timed_out") || text.contains("status: running"),
        "expected timed_out/running for silent child: {text}"
    );
    assert!(
        !text.contains("stdout:\nlate"),
        "must not wait for late output: {text}"
    );
    // Stronger: the echo has not landed in the returned stdout section. (The
    // status hint can legitimately contain the word "late", so parse the body.)
    if let Some(rest) = text.split("stdout:\n").nth(1) {
        let body = rest.split("stderr:\n").next().unwrap_or(rest);
        assert!(
            !body.contains("late"),
            "should not have late output yet: {text}"
        );
    }

    // Process must still be live in the session table.
    let list = dispatch_json(harness.clone(), json!({"action": "list"})).await;
    assert!(
        result_text(&list).contains(&id) && result_text(&list).contains("running"),
        "session should still be running in background: {}",
        result_text(&list)
    );

    let _ = close(&harness, &id).await;
}

/// Prompt-time summaries list only the caller's live sessions, one line
/// each; exited-but-unclosed and foreign-owned sessions are excluded.
#[tokio::test]
async fn running_tool_summaries_list_live_sessions_for_owner_only() {
    let service = Arc::new(BashService::new());
    let owner = uuid::Uuid::new_v4();
    let id = unique_id("summary");

    let start = dispatch_json_as(
        &service,
        owner,
        json!({
            "action": "start",
            "session_id": id,
            "command": "bash -c 'sleep 30'",
            "timeout_ms": 500,
            "idle_ms": 100,
        }),
    )
    .await;
    assert!(!start.is_error, "start: {}", result_text(&start));

    let lines = service.running_tool_summaries(owner);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains(&id), "{lines:?}");
    assert!(lines[0].contains("sleep 30"), "{lines:?}");
    assert!(!lines[0].contains('\n'), "must be one line: {lines:?}");
    assert!(
        service
            .running_tool_summaries(uuid::Uuid::new_v4())
            .is_empty(),
        "other agents must not see this session"
    );

    let closed = dispatch_json_as(
        &service,
        owner,
        json!({"action": "close", "session_id": id}),
    )
    .await;
    assert!(!closed.is_error, "close: {}", result_text(&closed));
    assert!(
        service.running_tool_summaries(owner).is_empty(),
        "closed sessions must not be listed"
    );

    // An exited-but-unclosed session is not running. Exit is observed by
    // an async waiter, so poll briefly instead of asserting immediately.
    let id2 = unique_id("exited");
    let start = dispatch_json_as(
        &service,
        owner,
        json!({
            "action": "start",
            "session_id": id2,
            "command": "true",
        }),
    )
    .await;
    assert!(!start.is_error, "start: {}", result_text(&start));
    let deadline = Instant::now() + Duration::from_secs(5);
    while !service.running_tool_summaries(owner).is_empty() {
        assert!(
            Instant::now() < deadline,
            "exited session still listed: {:?}",
            service.running_tool_summaries(owner)
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Summary lines stay compact: cmdlines are first-line-only and capped,
/// ages render as s/m/h.
#[test]
fn summary_cmdline_and_age_stay_compact() {
    assert_eq!(summary_cmdline("npm run dev"), "npm run dev");
    assert_eq!(summary_cmdline("line one\nline two"), "line one");
    let long = "x".repeat(100);
    let capped = summary_cmdline(&long);
    assert!(capped.chars().count() <= 61, "{capped}");
    assert!(capped.ends_with('…'), "{capped}");

    assert_eq!(brief_age(Duration::from_secs(42)), "42s");
    assert_eq!(brief_age(Duration::from_secs(420)), "7m");
    assert_eq!(brief_age(Duration::from_secs(2 * 3600 + 5 * 60)), "2h05m");
}

/// One-shot exec waits for exit but must not hang forever on a long sleep.
#[tokio::test]
async fn exec_timeout_kills_runaway() {
    let harness = harness();
    let t0 = Instant::now();
    let result = dispatch_json(
        harness,
        json!({
            "action": "exec",
            "command": "sleep 30",
            "timeout_ms": 500,
        }),
    )
    .await;
    let elapsed = t0.elapsed();
    assert!(!result.is_error, "{}", result_text(&result));
    assert!(
        elapsed < Duration::from_secs(3),
        "exec should time out near 500ms, took {elapsed:?}: {}",
        result_text(&result)
    );
    let text = result_text(&result);
    assert!(
        text.contains("timed_out") || text.contains("timed out"),
        "expected timeout status: {text}"
    );
}

/// A backgrounded grandchild inherits the stdout/stderr pipes, so the
/// readers see no EOF at child exit. Exec must still return promptly
/// (bounded drain), with the output the child did produce.
#[tokio::test]
async fn exec_returns_at_exit_despite_backgrounded_pipe_holder() {
    let harness = harness();
    let t0 = Instant::now();
    let result = dispatch_json(
        harness,
        json!({
            "action": "exec",
            "command": "sleep 15 & echo hi",
            "timeout_ms": 30_000,
        }),
    )
    .await;
    let elapsed = t0.elapsed();
    assert!(!result.is_error, "{}", result_text(&result));
    let text = result_text(&result);
    assert!(
        elapsed < Duration::from_secs(5),
        "exec should return at child exit + drain grace, took {elapsed:?}: {text}"
    );
    assert!(text.contains("hi"), "partial output must survive: {text}");
    assert!(text.contains("Exit code: Some(0)"), "{text}");
}

/// Exec children get a null stdin. Inheriting ours is never right: in
/// `--mode host` it is the NDJSON protocol pipe, and a child that reads
/// it desyncs the whole host connection.
#[tokio::test]
async fn exec_stdin_is_null_not_inherited() {
    let harness = harness();
    let t0 = Instant::now();
    let result = dispatch_json(
        harness,
        json!({
            "action": "exec",
            "command": "wc -c",
            "timeout_ms": 10_000,
        }),
    )
    .await;
    let elapsed = t0.elapsed();
    assert!(!result.is_error, "{}", result_text(&result));
    let text = result_text(&result);
    assert!(
        elapsed < Duration::from_secs(3),
        "stdin must be closed (instant EOF), took {elapsed:?}: {text}"
    );
    assert!(
        text.contains("0"),
        "wc -c on null stdin reads 0 bytes: {text}"
    );
}

/// Timeout must kill the whole process group, not just the outer `bash -c`.
///
/// Without process-group kill, a command like `bash -c 'sleep 30; …'` leaves
/// the grandchild `sleep` orphaned under init after we SIGKILL only bash.
#[tokio::test]
async fn exec_timeout_kills_process_group_not_just_bash() {
    let harness = harness();
    let dir = temp_dir("timeout-orphan");
    let marker = dir.path().join("still-alive.marker");
    let marker_s = marker.to_string_lossy().into_owned();
    // Unique sleep arg so we can find the grandchild without matching other tests.
    let sleep_tag = format!("17.{}", uuid::Uuid::new_v4().as_u128() % 100_000);
    let command = format!("sleep {sleep_tag}; echo still-alive > {marker_s}");

    let t0 = Instant::now();
    let result = dispatch_json(
        harness,
        json!({
            "action": "exec",
            "command": command,
            "timeout_ms": 400,
        }),
    )
    .await;
    let elapsed = t0.elapsed();
    assert!(!result.is_error, "{}", result_text(&result));
    assert!(
        elapsed < Duration::from_secs(3),
        "exec should time out near 400ms, took {elapsed:?}"
    );
    assert!(
        result_text(&result).contains("timed_out") || result_text(&result).contains("timed out"),
        "{}",
        result_text(&result)
    );

    // Give a reaped orphan a moment to reparent / finish if kill failed.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Grandchild must not still be running.
    let ps = myco_core::external_command::PS
        .command()
        .args(["-ax", "-o", "pid=,command="])
        .output()
        .expect("ps");
    let ps_text = String::from_utf8_lossy(&ps.stdout);
    assert!(
        !ps_text
            .lines()
            .any(|l| l.contains(&format!("sleep {sleep_tag}"))),
        "grandchild sleep should have been process-group killed; still running:\n{ps_text}"
    );
    assert!(
        !marker.exists(),
        "marker must not be written after timeout (orphan finished the command)"
    );
}

#[tokio::test]
async fn exec_cancel_kills_runaway() {
    let harness = harness();
    let cancel = myco_core::CancelToken::new();
    let cancel2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        cancel2.cancel();
    });
    let t0 = Instant::now();
    let result = harness
        .dispatch_tool_use(
            tool_use_json(json!({
                "action": "exec",
                "command": "sleep 30",
                "timeout_ms": 10_000,
            })),
            HostDispatchContext::new(uuid::Uuid::nil(), cancel),
        )
        .await;
    let elapsed = t0.elapsed();
    assert!(
        elapsed < Duration::from_secs(3),
        "cancel should kill exec quickly, took {elapsed:?}: {}",
        result_text(&result)
    );
    assert!(result.is_error, "cancelled exec should be an error result");
    assert!(
        result_text(&result).contains("cancelled"),
        "{}",
        result_text(&result)
    );
}

#[tokio::test]
async fn echo_stdout() {
    let result = dispatch_json(harness(), json!({"command": "echo hello-from-bash"})).await;
    assert!(!result.is_error, "{}", result_text(&result));
    let text = result_text(&result);
    assert!(text.contains("hello-from-bash"), "{text}");
    assert!(text.contains("Exit code: Some(0)"), "{text}");
    assert!(text.contains("stdout:"), "{text}");
}

#[tokio::test]
async fn nonzero_exit_still_ok_result() {
    let result = dispatch_json(harness(), json!({"command": "exit 7"})).await;
    assert!(!result.is_error, "{}", result_text(&result));
    let text = result_text(&result);
    assert!(text.contains("Exit code: Some(7)"), "{text}");
}

#[tokio::test]
async fn stderr_captured() {
    let result = dispatch_json(harness(), json!({"command": "echo err-msg 1>&2"})).await;
    assert!(!result.is_error, "{}", result_text(&result));
    let text = result_text(&result);
    assert!(text.contains("err-msg"), "{text}");
    assert!(text.contains("stderr:"), "{text}");
}

#[tokio::test]
async fn exec_respects_cwd() {
    let dir = temp_dir("cwd");
    let dir_str = dir.path().to_string_lossy().into_owned();

    let result = dispatch_json(
        harness(),
        json!({
            "command": "pwd",
            "cwd": &dir_str,
        }),
    )
    .await;
    assert!(!result.is_error, "{}", result_text(&result));
    let text = result_text(&result);
    // macOS /var is often a symlink to /private/var; compare canonical paths.
    let expected = std::fs::canonicalize(dir.path()).unwrap();
    let expected_s = expected.to_string_lossy();
    assert!(
        text.contains(expected_s.as_ref()) || text.contains(&dir_str),
        "expected pwd under {expected_s} or {dir_str}: {text}"
    );
}

/// Blocking dispatch path: an over-max timeout must error immediately —
/// not clamp, and not start the process first.
async fn assert_dispatch_rejects_over_max(value: serde_json::Value, max_ms: u64) {
    let t0 = Instant::now();
    let result = dispatch_json(harness(), value).await;
    let elapsed = t0.elapsed();
    assert!(
        result.is_error,
        "expected tool error, got: {}",
        result_text(&result)
    );
    let text = result_text(&result);
    assert!(
        text.contains("exceeds max") && text.contains(&max_ms.to_string()),
        "{text}"
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "reject must be immediate, took {elapsed:?}: {text}"
    );
}

#[tokio::test]
async fn dispatch_rejects_exec_timeout_above_max() {
    assert_dispatch_rejects_over_max(
        json!({
            "action": "exec",
            "command": "sleep 30",
            "timeout_ms": 1_800_001,
        }),
        MAX_EXEC_TIMEOUT_MS,
    )
    .await;
}

#[tokio::test]
async fn dispatch_rejects_session_timeout_above_max() {
    assert_dispatch_rejects_over_max(
        json!({
            "action": "start",
            "session_id": unique_id("tmax"),
            "command": "bash --noprofile --norc",
            "timeout_ms": 1_800_001,
        }),
        MAX_TIMEOUT_MS,
    )
    .await;
}

#[tokio::test]
async fn session_start_respects_cwd() {
    let harness = harness();
    let id = unique_id("cwd");
    let dir = temp_dir("sess-cwd");
    let dir_str = dir.path().to_string_lossy().into_owned();

    let start = dispatch_json(
        harness.clone(),
        json!({
            "action": "start",
            "session_id": id,
            "command": "bash --noprofile --norc",
            "cwd": &dir_str,
            "idle_ms": 200,
            "timeout_ms": 1000,
        }),
    )
    .await;
    assert!(!start.is_error, "start: {}", result_text(&start));

    let text = write_ok(&harness, &id, "pwd\n").await;
    let expected = std::fs::canonicalize(dir.path()).unwrap();
    let expected_s = expected.to_string_lossy();
    assert!(
        text.contains(expected_s.as_ref()) || text.contains(&dir_str),
        "session should start in cwd: {text}"
    );

    let _ = close(&harness, &id).await;
}

#[tokio::test]
async fn session_cat_roundtrip() {
    let harness = harness();
    let id = unique_id("cat");

    let start_text = start_ok(&harness, &id, "cat").await;
    assert!(
        start_text.contains(&format!("session_id: {id}")),
        "{start_text}"
    );

    let write_text = write_ok(&harness, &id, "hello-session\n").await;
    assert!(
        write_text.contains("hello-session"),
        "expected echo from cat: {write_text}"
    );

    let list = dispatch_json(harness.clone(), json!({"action": "list"})).await;
    assert!(!list.is_error, "list: {}", result_text(&list));
    assert!(result_text(&list).contains(&id), "{}", result_text(&list));

    let close_result = close(&harness, &id).await;
    assert!(
        !close_result.is_error,
        "close: {}",
        result_text(&close_result)
    );
    assert!(
        result_text(&close_result).contains("session closed"),
        "{}",
        result_text(&close_result)
    );

    let list2 = dispatch_json(harness, json!({"action": "list"})).await;
    assert!(
        result_text(&list2).contains("(no live sessions)") || !result_text(&list2).contains(&id),
        "{}",
        result_text(&list2)
    );
}

/// Interactive shell across multiple tool turns: state must persist.
#[tokio::test]
async fn session_interactive_shell_multi_turn() {
    let harness = harness();
    let id = unique_id("sh");

    // Non-interactive bash reading commands from stdin still keeps shell state.
    // Avoid `bash -i` here: prompts/job-control noise makes idle detection flaky in CI.
    start_ok(&harness, &id, "bash --noprofile --norc").await;

    write_ok(&harness, &id, "export MYCO_MULTI_TURN=alive-from-turn-1\n").await;

    let turn2_text = write_ok(&harness, &id, "printf 'saw=%s\\n' \"$MYCO_MULTI_TURN\"\n").await;
    assert!(
        turn2_text.contains("saw=alive-from-turn-1"),
        "shell state must persist across writes: {turn2_text}"
    );

    let turn3_text = write_ok(&harness, &id, "echo turn-3-still-here\n").await;
    assert!(
        turn3_text.contains("turn-3-still-here"),
        "third turn should still talk to the same shell: {turn3_text}"
    );

    let list = dispatch_json(harness.clone(), json!({"action": "list"})).await;
    assert!(
        result_text(&list).contains(&id) && result_text(&list).contains("running"),
        "session should still be live after multi-turn use: {}",
        result_text(&list)
    );

    let close_result = close(&harness, &id).await;
    assert!(
        !close_result.is_error,
        "close: {}",
        result_text(&close_result)
    );
}

#[tokio::test]
async fn session_python_repl() {
    let harness = harness();
    let id = unique_id("py");

    // Python's banner/prompt land on stderr and can be slow; give the REPL a
    // longer idle window than the shell tests.
    let start = dispatch_json(
        harness.clone(),
        json!({
            "action": "start",
            "session_id": id,
            "command": "python3 -u -i",
            "idle_ms": 400,
            "timeout_ms": 1000,
        }),
    )
    .await;
    assert!(!start.is_error, "start: {}", result_text(&start));

    let write = dispatch_json(
        harness.clone(),
        json!({
            "action": "write",
            "session_id": id,
            "stdin": "print(6*7)\n",
            "idle_ms": 400,
            "timeout_ms": 1000,
        }),
    )
    .await;
    assert!(!write.is_error, "write: {}", result_text(&write));
    // Unambiguous evaluation: the REPL must have computed 6*7, not merely
    // echoed banner digits.
    let write_text = result_text(&write);
    assert!(
        write_text.lines().any(|l| l.trim() == "42"),
        "expected python to evaluate 6*7 to a `42` line: {write_text}"
    );

    let close_result = close(&harness, &id).await;
    assert!(
        !close_result.is_error,
        "close: {}",
        result_text(&close_result)
    );
}

#[tokio::test]
async fn session_duplicate_id_rejected() {
    let harness = harness();
    let id = unique_id("dup");

    start_ok(&harness, &id, "cat").await;

    let start2 = dispatch_json(
        harness.clone(),
        json!({
            "action": "start",
            "session_id": id,
            "command": "cat",
        }),
    )
    .await;
    assert!(start2.is_error, "duplicate should error");
    assert!(
        result_text(&start2).contains("already exists"),
        "{}",
        result_text(&start2)
    );

    let _ = close(&harness, &id).await;
}

#[tokio::test]
async fn session_unknown_write_errors() {
    let harness = harness();
    let result = dispatch_json(
        harness,
        json!({
            "action": "write",
            "session_id": "no-such-session",
            "stdin": "x\n",
        }),
    )
    .await;
    assert!(result.is_error);
    assert!(
        result_text(&result).contains("unknown session"),
        "{}",
        result_text(&result)
    );
}

#[tokio::test]
async fn session_byte_cap_truncates() {
    let harness = harness();
    let id = unique_id("big");

    start_ok(&harness, &id, "cat").await;

    // Write more than max_bytes; cat will echo it all.
    let payload = "x".repeat(200);
    let write = dispatch_json(
        harness.clone(),
        json!({
            "action": "write",
            "session_id": id,
            "stdin": payload,
            "timeout_ms": 1000,
            "idle_ms": 300,
            "max_bytes": 50,
        }),
    )
    .await;
    assert!(!write.is_error, "write: {}", result_text(&write));
    let text = result_text(&write);
    assert!(
        text.contains("truncated") || text.contains("bytes_returned"),
        "{text}"
    );

    // Follow-up read may get the rest.
    let read = dispatch_json(
        harness.clone(),
        json!({
            "action": "read",
            "session_id": id,
            "timeout_ms": 1000,
            "idle_ms": 200,
            "max_bytes": 500,
        }),
    )
    .await;
    assert!(!read.is_error, "read: {}", result_text(&read));

    let _ = close(&harness, &id).await;
}

#[tokio::test]
async fn session_exited_process_reports_status() {
    let harness = harness();
    let id = unique_id("exit");

    let text = start_ok(&harness, &id, "bash -c 'echo bye; exit 3'").await;
    assert!(text.contains("bye"), "{text}");
    assert!(
        text.contains("exited") || text.contains("running"),
        "{text}"
    );

    let read = dispatch_json(
        harness.clone(),
        json!({
            "action": "read",
            "session_id": id,
            "timeout_ms": 1000,
            "idle_ms": 100,
        }),
    )
    .await;
    let read_text = result_text(&read);
    assert!(
        read_text.contains("exited") || text.contains("exited"),
        "start={text}\nread={read_text}"
    );
    assert!(
        read_text.contains("exit_code: Some(3)") || text.contains("exit_code: Some(3)"),
        "start={text}\nread={read_text}"
    );

    let _ = close(&harness, &id).await;
}

#[tokio::test]
async fn session_foreign_owner_rejected() {
    let service = Arc::new(BashService::new());
    let owner_a = uuid::Uuid::new_v4();
    let owner_b = uuid::Uuid::new_v4();
    let id = unique_id("own");

    let start = dispatch_json_as(
        &service,
        owner_a,
        json!({
            "action": "start",
            "session_id": id,
            "command": "cat",
            "timeout_ms": 1000,
            "idle_ms": 100,
        }),
    )
    .await;
    assert!(!start.is_error, "start: {}", result_text(&start));
    assert!(
        result_text(&start).contains("owner:"),
        "{}",
        result_text(&start)
    );

    // Different agent cannot write.
    let write = dispatch_json_as(
        &service,
        owner_b,
        json!({
            "action": "write",
            "session_id": id,
            "stdin": "nope\n",
        }),
    )
    .await;
    assert!(write.is_error, "foreign write should fail");
    assert!(
        result_text(&write).contains("owned by another agent"),
        "{}",
        result_text(&write)
    );

    // Owner can still write.
    let owner_write = dispatch_json_as(
        &service,
        owner_a,
        json!({
            "action": "write",
            "session_id": id,
            "stdin": "yep\n",
            "timeout_ms": 1000,
            "idle_ms": 200,
        }),
    )
    .await;
    assert!(
        !owner_write.is_error,
        "owner write: {}",
        result_text(&owner_write)
    );
    assert!(
        result_text(&owner_write).contains("yep"),
        "{}",
        result_text(&owner_write)
    );

    let _ = dispatch_json_as(
        &service,
        owner_a,
        json!({"action": "close", "session_id": id}),
    )
    .await;
}

#[tokio::test]
async fn agent_drop_reaps_owned_sessions() {
    let service = Arc::new(BashService::new());
    let harness = Arc::new(HostWorker::new(
        "test",
        vec![service.clone() as Arc<dyn ToolService>],
    ));
    let agent_id = uuid::Uuid::new_v4();
    let id = unique_id("reap");

    // Start a long-lived session as this agent.
    let start = dispatch_json_as(
        &service,
        agent_id,
        json!({
            "action": "start",
            "session_id": id,
            "command": "cat",
            "timeout_ms": 1000,
            "idle_ms": 100,
        }),
    )
    .await;
    assert!(!start.is_error, "start: {}", result_text(&start));

    // Session is live.
    {
        let sessions = service.sessions.lock().unwrap();
        assert!(sessions.contains_key(&id), "session should be live");
        assert_eq!(sessions.get(&id).unwrap().owner, agent_id);
    }

    // Agent teardown reaches the service through the harness broadcast
    // (Agent::drop → notify_agent_finished) and reaps the session.
    harness.notify_agent_finished(agent_id);

    {
        let sessions = service.sessions.lock().unwrap();
        assert!(
            !sessions.contains_key(&id),
            "session should be reaped on agent finish"
        );
    }
}

// --- signal ------------------------------------------------------------------

#[test]
fn signal_defaults_to_int_and_requires_session_id() {
    let input: Input = serde_json::from_value(json!({
        "action": "signal",
        "session_id": "s",
    }))
    .unwrap();
    match resolve_action(&input).unwrap() {
        Action::Signal { signal, .. } => assert_eq!(signal, SignalKind::Int),
        other => panic!("expected Signal, got {other:?}"),
    }

    let input: Input = serde_json::from_value(json!({
        "action": "signal",
        "signal": "term",
        "session_id": "s",
    }))
    .unwrap();
    match resolve_action(&input).unwrap() {
        Action::Signal { signal, .. } => assert_eq!(signal, SignalKind::Term),
        other => panic!("expected Signal, got {other:?}"),
    }

    let input: Input = serde_json::from_value(json!({"action": "signal"})).unwrap();
    let err = resolve_action(&input).unwrap_err();
    assert!(err.contains("signal requires `session_id`"), "{err}");
}

/// A child with no SIGINT handler dies on the signal (`exit_signal: 2`), but
/// the session entry survives for inspection — `close` is what reaps it.
#[tokio::test]
async fn signal_int_interrupts_running_command_without_reaping_session() {
    let harness = harness();
    let id = unique_id("sig");

    start_ok(&harness, &id, "bash -c 'sleep 30; echo late'").await;

    let t0 = Instant::now();
    let signal = dispatch_json(
        harness.clone(),
        json!({
            "action": "signal",
            "session_id": id,
            "timeout_ms": 2_000,
            "idle_ms": 200,
        }),
    )
    .await;
    assert!(!signal.is_error, "signal: {}", result_text(&signal));
    let text = result_text(&signal);
    assert!(
        text.contains("SIGINT"),
        "result should name the delivered signal: {text}"
    );
    assert!(
        !text.contains("stdout:\nlate"),
        "interrupted sleep must not reach its `echo late`: {text}"
    );
    assert!(
        text.contains("exit_signal: Some(2)"),
        "child without a handler should die on SIGINT: {text}"
    );
    assert!(
        t0.elapsed() < Duration::from_secs(5),
        "signal must not wait out the 30s sleep, took {:?}",
        t0.elapsed()
    );

    // Unlike `close`, the session stays in the table.
    let list = dispatch_json(harness.clone(), json!({"action": "list"})).await;
    assert!(
        result_text(&list).contains(&id),
        "signal must not reap the session: {}",
        result_text(&list)
    );

    let _ = close(&harness, &id).await;
}

/// The nested-agent case: a child that installs a SIGINT handler (as `myco`
/// does, to cancel its in-flight turn) survives the signal and keeps reading
/// stdin — so a supervisor can interrupt a child and carry on driving it.
#[tokio::test]
async fn signal_int_leaves_handler_installed_child_alive_and_writable() {
    let harness = harness();
    let id = unique_id("sighandler");

    start_ok(
        &harness,
        &id,
        "bash -c 'trap \"echo INTERRUPTED\" INT; while read -r line; do echo \"got:$line\"; done'",
    )
    .await;

    let signal = dispatch_json(
        harness.clone(),
        json!({
            "action": "signal",
            "session_id": id,
            "timeout_ms": 2_000,
            "idle_ms": 300,
        }),
    )
    .await;
    assert!(!signal.is_error, "signal: {}", result_text(&signal));
    let text = result_text(&signal);
    assert!(
        text.contains("INTERRUPTED"),
        "child's SIGINT handler should have run: {text}"
    );
    assert!(
        !text.contains("status: exited"),
        "child handling SIGINT must stay alive: {text}"
    );

    // Still driveable after the interrupt — the reason `signal` exists.
    let write_text = write_ok(&harness, &id, "still-there\n").await;
    assert!(
        write_text.contains("got:still-there"),
        "session should still accept stdin after SIGINT: {write_text}"
    );

    let _ = close(&harness, &id).await;
}

#[tokio::test]
async fn signal_on_exited_session_reports_error() {
    let harness = harness();
    let id = unique_id("sigexit");

    start_ok(&harness, &id, "bash -c 'echo bye'").await;

    let signal = dispatch_json(
        harness.clone(),
        json!({"action": "signal", "session_id": id}),
    )
    .await;
    assert!(signal.is_error, "expected error: {}", result_text(&signal));
    assert!(
        result_text(&signal).contains("already exited"),
        "{}",
        result_text(&signal)
    );

    let _ = close(&harness, &id).await;
}

#[tokio::test]
async fn signal_rejects_foreign_owner() {
    let service = Arc::new(BashService::new());
    let owner = uuid::Uuid::new_v4();
    let id = unique_id("sigown");

    let start = dispatch_json_as(
        &service,
        owner,
        json!({
            "action": "start",
            "session_id": id,
            "command": "bash -c 'sleep 30'",
            "timeout_ms": 500,
            "idle_ms": 100,
        }),
    )
    .await;
    assert!(!start.is_error, "start: {}", result_text(&start));

    let signal = dispatch_json_as(
        &service,
        uuid::Uuid::new_v4(),
        json!({"action": "signal", "session_id": id}),
    )
    .await;
    assert!(signal.is_error, "expected error: {}", result_text(&signal));
    assert!(
        result_text(&signal).contains("owned by another agent"),
        "{}",
        result_text(&signal)
    );

    service.reap_owner(owner);
}

/// The advertised schema says `additionalProperties: false`; the struct must
/// agree. A gateway that skips schema validation would otherwise let a typo'd
/// field through, and the call would half-execute against defaults instead of
/// telling the model it got the field name wrong.
#[tokio::test]
async fn a_typoed_field_is_rejected_not_silently_dropped() {
    let harness = harness();
    let result = dispatch_json(
        harness,
        // `timeout_millis` is not the field name; `timeout_ms` is.
        json!({"command": "echo hi", "timeout_millis": 500}),
    )
    .await;
    assert!(
        result.is_error,
        "unknown field should be an error: {}",
        result_text(&result)
    );
    assert!(
        result_text(&result).contains("timeout_millis"),
        "error should name the offending field: {}",
        result_text(&result)
    );
}

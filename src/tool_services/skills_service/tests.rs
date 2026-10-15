use super::*;
use crate::core::CancelToken;
use crate::host::HostWorker;
use crate::test_support::{result_text, temp_dir};
use crate::tool_services::TextEditorService;
use serde_json::json;

fn service(directory: &std::path::Path) -> Arc<SkillsService> {
    Arc::new(SkillsService {
        directory: Ok(directory.to_path_buf()),
        home: None,
        owners: Mutex::new(HashMap::new()),
    })
}

fn skill(directory: &std::path::Path, description: &str) -> PathBuf {
    let path = directory.join(".agents/skills/project/SKILL.md");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        format!("---\nname: project\ndescription: {description}\n---\nPRIVATE_INSTRUCTIONS\n"),
    )
    .unwrap();
    path
}

fn context() -> HostDispatchContext {
    HostDispatchContext::new(uuid::Uuid::new_v4(), CancelToken::new())
}

fn view(path: &std::path::Path) -> ToolUse {
    ToolUse {
        name: "str_replace_based_edit_tool".into(),
        input: json!({"command":"view", "path":path}),
    }
}

#[test]
fn in_process_discovery_supports_an_executor_without_tokio() {
    let temp = temp_dir("skill-sync");
    skill(temp.path(), "synchronous embedding");
    let result = futures::executor::block_on(service(temp.path()).dispatch_tool_use(
        ToolUse {
            name: "skills".into(),
            input: json!({}),
        },
        context(),
    ));
    assert!(!result.is_error);
    assert!(result_text(&result).contains("synchronous embedding"));
}

#[tokio::test]
async fn cancellation_leaves_the_scan_gate_with_unfinished_blocking_io() {
    let gate = Arc::new(tokio::sync::Mutex::new(()));
    let guard = gate.clone().lock_owned().await;
    let (started, waiting) = tokio::sync::oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let cancel = CancelToken::new();
    let signal = cancel.clone();
    let job = tokio::spawn(async move {
        run_scan(&signal, move || {
            let _guard = guard;
            started.send(()).unwrap();
            blocked
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            Ok(())
        })
        .await
    });
    waiting.await.unwrap();
    cancel.cancel();
    assert!(job.await.unwrap().unwrap_err().contains("cancelled"));
    let still_locked = gate.try_lock().is_err();
    release.send(()).unwrap();
    assert!(
        still_locked,
        "cancellation permitted overlapping blocking scans"
    );
    drop(
        tokio::time::timeout(std::time::Duration::from_secs(2), gate.lock())
            .await
            .unwrap(),
    );
}

#[tokio::test]
async fn notices_follow_metadata_changes_and_removal_without_repeating() {
    let temp = temp_dir("skill-notices");
    let path = skill(temp.path(), "first description");
    let service = service(temp.path());
    let ctx = context();
    let call = view(temp.path());
    let first = service
        .clone()
        .observe_successful_call(call.clone(), ctx.clone())
        .await
        .unwrap();
    assert!(first.contains("first description"), "{first}");
    assert!(!first.contains("PRIVATE_INSTRUCTIONS"));
    assert!(
        service
            .clone()
            .observe_successful_call(call.clone(), ctx.clone())
            .await
            .is_none()
    );
    skill(temp.path(), "second description");
    let changed = service
        .clone()
        .observe_successful_call(call.clone(), ctx.clone())
        .await
        .unwrap();
    assert!(changed.contains("second description"), "{changed}");
    std::fs::remove_file(path).unwrap();
    let removed = service
        .clone()
        .observe_successful_call(call.clone(), ctx.clone())
        .await
        .unwrap();
    assert!(!removed.contains("second description"), "{removed}");
    assert!(service.observe_successful_call(call, ctx).await.is_none());
}

#[tokio::test]
async fn new_owner_and_new_thread_receive_the_catalog_and_cleanup_releases_cache() {
    let temp = temp_dir("skill-owner");
    skill(temp.path(), "scoped metadata");
    let service = service(temp.path());
    let mut ctx = context();
    ctx.thread_id = Some("predecessor".into());
    let call = view(temp.path());
    assert!(
        service
            .clone()
            .observe_successful_call(call.clone(), ctx.clone())
            .await
            .is_some()
    );
    assert!(
        service
            .clone()
            .observe_successful_call(call.clone(), ctx.clone())
            .await
            .is_none()
    );
    ctx.thread_id = Some("successor".into());
    assert!(
        service
            .clone()
            .observe_successful_call(call.clone(), ctx.clone())
            .await
            .is_some()
    );
    assert!(
        service
            .clone()
            .observe_successful_call(call.clone(), context())
            .await
            .is_some()
    );
    assert!(service.resources(ctx.agent_id).is_empty());
    assert!(service.running_tool_summaries(ctx.agent_id).is_empty());
    service.on_agent_finished(ctx.agent_id);
    assert!(!service.owners.lock().unwrap().contains_key(&ctx.agent_id));
    assert!(service.observe_successful_call(call, ctx).await.is_some());
}

#[tokio::test]
async fn host_observes_only_successful_file_calls_and_preserves_their_result() {
    let temp = temp_dir("skill-host");
    let project = temp.path().join("project");
    skill(&project, "host-local metadata");
    let discovery = service(temp.path());
    let worker = Arc::new(HostWorker::new(
        "remote-fixture",
        vec![Arc::new(TextEditorService::new()), discovery],
    ));
    let ctx = context();
    let failed = worker
        .dispatch_tool_use(view(&project.join("missing")), ctx.clone())
        .await;
    assert!(failed.is_error);
    assert_eq!(failed.content.len(), 1);
    let path = project.join("file.txt");
    std::fs::write(&path, "file content").unwrap();
    let result = worker.dispatch_tool_use(view(&path), ctx.clone()).await;
    assert!(!result.is_error);
    let text = result_text(&result);
    assert!(text.contains("file content"));
    assert!(text.contains("remote-fixture"));
    assert!(text.contains("host-local metadata"));
    let repeated = worker.dispatch_tool_use(view(&path), ctx).await;
    assert!(!result_text(&repeated).contains("host-local metadata"));
}

#[tokio::test]
async fn explicit_scans_resolve_on_the_host_and_shell_text_is_not_parsed() {
    let temp = temp_dir("skill-explicit");
    let project = temp.path().join("other");
    skill(&project, "other directory");
    let service = service(temp.path());
    let ctx = context();
    let bash = ToolUse {
        name: "bash".into(),
        input: json!({"command":format!("cd {} && true", project.display())}),
    };
    assert!(
        service
            .clone()
            .observe_successful_call(bash, ctx.clone())
            .await
            .is_none()
    );
    let call = ToolUse {
        name: "skills".into(),
        input: json!({"path":"other"}),
    };
    for _ in 0..2 {
        let result = service
            .clone()
            .dispatch_tool_use(call.clone(), ctx.clone())
            .await;
        assert!(!result.is_error);
        assert!(result_text(&result).contains("other directory"));
    }
    let invalid = ToolUse {
        name: "skills".into(),
        input: json!({"path":""}),
    };
    assert!(service.dispatch_tool_use(invalid, ctx).await.is_error);
}

#[tokio::test]
async fn concurrent_calls_dedupe_and_cancelled_scans_do_not_consume_notices() {
    let temp = temp_dir("skill-concurrent");
    skill(temp.path(), "visible once");
    let service = service(temp.path());
    let ctx = context();
    ctx.cancel.cancel();
    let call = view(temp.path());
    assert!(
        service
            .clone()
            .observe_successful_call(call.clone(), ctx.clone())
            .await
            .is_none()
    );
    let mut ctx = ctx;
    ctx.cancel = CancelToken::new();
    let (first, second) = tokio::join!(
        service
            .clone()
            .observe_successful_call(call.clone(), ctx.clone()),
        service.clone().observe_successful_call(call, ctx),
    );
    assert_ne!(first.is_some(), second.is_some());
}

#[tokio::test]
async fn owner_observation_cache_has_a_fixed_directory_bound() {
    let temp = temp_dir("skill-cache-bound");
    let service = service(temp.path());
    let ctx = context();
    for index in 0..MAX_OBSERVED_DIRECTORIES + 5 {
        let dir = temp.path().join(index.to_string());
        std::fs::create_dir(&dir).unwrap();
        service
            .clone()
            .observe_successful_call(view(&dir), ctx.clone())
            .await;
    }
    let observations = service.observations(ctx.agent_id);
    let observations = observations.lock().await;
    assert_eq!(observations.directories.len(), MAX_OBSERVED_DIRECTORIES);
    assert!(observations.directories.front().unwrap().0.ends_with("5"));
}

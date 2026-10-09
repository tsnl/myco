//! A conversation thread is replaceable; its remote execution owner is not.

mod test_utils;

use std::sync::Arc;

use myco::chat::{CompactWorkerError, Compactor, SessionRunner};
use myco::core::{Async, CancelToken};
use myco::generative_model::{Content, GenerateOutput, ToolUse, TurnEndReason};
use myco::harness::{Harness, HarnessConfig, HostConfig};
use myco::session::{ActiveSession, CompactOutcome, Session, Thread, compact_thread};
use myco::{Agent, NullEventSink, SessionRuntime};
use serde_json::json;

struct Summarizer {
    fail: bool,
    cancel: bool,
}

impl Compactor for Summarizer {
    fn compact(
        self: Arc<Self>,
        session: Session,
        cancel: CancelToken,
    ) -> Async<Result<(Thread, CompactOutcome), CompactWorkerError>> {
        Box::pin(async move {
            if self.fail {
                return Err(CompactWorkerError::Failed("scripted failure".into()));
            }
            let result = compact_thread(&session, "Keep working in the retained remote shell")
                .map_err(CompactWorkerError::Failed)?;
            if self.cancel {
                cancel.cancel();
            }
            Ok(result)
        })
    }
}

fn output(input: Option<serde_json::Value>) -> GenerateOutput {
    GenerateOutput {
        content: vec![Content::Text {
            text: "continue".into(),
        }],
        turn_end_reason: if input.is_some() {
            TurnEndReason::ToolUse
        } else {
            TurnEndReason::EndTurn
        },
        tool_uses: input
            .into_iter()
            .map(|input| ToolUse {
                name: "bash".into(),
                input,
            })
            .collect(),
        usage: None,
    }
}

async fn submit(runner: &mut SessionRunner) {
    runner
        .submit(
            vec![Content::Text {
                text: "continue".into(),
            }],
            chrono::Utc::now(),
            CancelToken::new(),
        )
        .await
        .result
        .unwrap();
}

async fn remote_shell(runner: &SessionRunner) -> myco::core::ToolResource {
    runner
        .runtime()
        .resources()
        .await
        .into_iter()
        .find(|host| host.host == "remote")
        .unwrap()
        .resources
        .unwrap()
        .into_iter()
        .find(|resource| resource.id == "keep")
        .unwrap()
}

#[test]
fn remote_shell_state_survives_repeated_compaction_failure_and_cancellation() {
    // Run the fixture in a child so MYCO_HOME is isolated without mutating the
    // test process environment while Rust's test runner has threads alive.
    if std::env::var_os("MYCO_REMOTE_CONTINUITY_FIXTURE").is_none() {
        let home =
            std::env::temp_dir().join(format!("myco-remote-continuity-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("remote_shell_state_survives_repeated_compaction_failure_and_cancellation")
            .arg("--nocapture")
            .env("MYCO_HOME", &home)
            .env("MYCO_REMOTE_CONTINUITY_FIXTURE", "1")
            .output()
            .unwrap();
        std::fs::remove_dir_all(home).unwrap();
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        return;
    }
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let harness = Harness::attach(HarnessConfig {
            remote_hosts: vec![HostConfig { name: "remote".into(), command: vec![env!("CARGO_BIN_EXE_myco").into(), "--mode".into(), "host".into()] }],
            host_idle_timeout_secs: 1,
            ..Default::default()
        }).await.unwrap();
        let mut scripts = vec![output(Some(json!({
            "host":"remote", "action":"start", "session_id":"keep", "command":"bash",
            "stdin":"counter=40\n", "idle_ms":10, "timeout_ms":100,
        }))), output(None)];
        for _ in 0..4 {
            scripts.extend([output(Some(json!({
                "host":"remote", "action":"write", "session_id":"keep",
                "stdin":"counter=$((counter + 1)); printf 'counter=%s\\n' \"$counter\"\n", "idle_ms":10, "timeout_ms":1000,
            }))), output(None)]);
        }
        let model = test_utils::ScriptedModel::new(scripts);
        let runtime = SessionRuntime::new(harness, ActiveSession::new(Session::new("remote-continuity")));
        let agent = Agent::new(model.clone(), runtime.clone(), Arc::new(NullEventSink));
        let mut runner = SessionRunner::new(agent, runtime).await.unwrap();
        submit(&mut runner).await;
        let initial = remote_shell(&runner).await;
        for (index, (fail, cancel)) in [(false, false), (false, false), (true, false), (false, true)].into_iter().enumerate() {
            let before = runner.runtime().session().snapshot().active_thread().id.clone();
            runner.set_compactor(Arc::new(Summarizer { fail, cancel }), None);
            let compacted = runner.compact(CancelToken::new()).await;
            assert_eq!(compacted.is_err(), fail || cancel);
            let after = runner.runtime().session().snapshot().active_thread().id.clone();
            assert_eq!(before == after, fail || cancel);
            submit(&mut runner).await;
            let current = remote_shell(&runner).await;
            assert_eq!(current.details["instance_id"], initial.details["instance_id"]);
            assert_eq!(current.details["pid"], initial.details["pid"]);
            let history = serde_json::to_string(runner.agent().history()).unwrap();
            assert!(history.contains(&format!("counter={}", 41 + index)), "{history}");
        }
        assert_eq!(model.remaining(), 0);
        assert_eq!(runner.runtime().session().snapshot().threads().len(), 3);
    });
}

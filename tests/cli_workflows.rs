//! Drive the shipped executable with an isolated profile and local scripted provider.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::Duration;

use myco::Session;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

mod test_utils;
use test_utils::StubHttpServer;

struct CliEnv {
    dir: PathBuf,
    config: PathBuf,
}

impl CliEnv {
    fn new(provider: &StubHttpServer, compact: bool) -> Self {
        let dir = std::env::temp_dir().join(format!("myco-cli-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.toml");
        std::fs::write(&config, format!(
            "model = \"pipetest\"\n[models.pipetest]\nprotocol = \"openai-responses\"\nbase_url = {:?}\nauth = {{ source = \"none\" }}\ncontext_window = 100000\n{}\n",
            provider.base_url(), if compact { "auto_compact_at = 0.8" } else { "" },
        )).unwrap();
        Self { dir, config }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_myco"));
        command
            .args(args)
            .current_dir(&self.dir)
            .env("MYCO_HOME", &self.dir)
            .env("MYCO_PROFILE", "default")
            .env("MYCO_CONFIG", &self.config)
            .env("TERM", "xterm")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }

    async fn run(&self, args: &[&str], input: &[u8]) -> Output {
        let mut child = self.command(args).spawn().unwrap();
        child.stdin.take().unwrap().write_all(input).await.unwrap();
        wait(child).await
    }

    fn session_path(&self, id: &str) -> PathBuf {
        self.dir
            .join("profiles/default/session")
            .join(&id[..2])
            .join(format!("{id}.json"))
    }

    fn saved(&self, id: &str) -> Session {
        serde_json::from_slice(&std::fs::read(self.session_path(id)).unwrap()).unwrap()
    }

    fn seed(&self) -> Session {
        let session = Session::new("pipetest");
        let path = self.session_path(&session.id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_vec(&session).unwrap()).unwrap();
        session
    }

    fn use_provider(&self, provider: &StubHttpServer) {
        let config = std::fs::read_to_string(&self.config).unwrap();
        let url = config
            .lines()
            .find(|line| line.starts_with("base_url"))
            .unwrap();
        std::fs::write(
            &self.config,
            config.replace(url, &format!("base_url = {:?}", provider.base_url())),
        )
        .unwrap();
    }

    fn retry(&self, policy: &str) {
        let original = std::fs::read_to_string(&self.config).unwrap();
        std::fs::write(
            &self.config,
            format!("{original}\n[models.pipetest.retry]\n{policy}\n"),
        )
        .unwrap();
    }
}

impl Drop for CliEnv {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn wait(child: Child) -> Output {
    tokio::time::timeout(Duration::from_secs(25), child.wait_with_output())
        .await
        .expect("CLI must not hang")
        .expect("wait for CLI")
}

fn success(output: &Output) -> String {
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn session_id(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .find_map(|line| line.strip_prefix("session="))
        .expect("saved session announced")
        .into()
}

fn answer(text: &str, tokens: u64) -> Vec<u8> {
    StubHttpServer::sse_response(vec![
        json!({"type":"response.output_text.delta", "delta":text}),
        json!({"type":"response.completed", "response":{
            "status":"completed", "usage":{"input_tokens":tokens,"output_tokens":1}
        }}),
    ])
}

fn tool(name: &str, input: Value) -> Vec<u8> {
    StubHttpServer::sse_response(vec![
        json!({"type":"response.output_item.added", "output_index":0,
            "item":{"type":"function_call", "name":name, "call_id":"tool", "arguments":""}}),
        json!({"type":"response.function_call_arguments.done", "output_index":0, "arguments":input.to_string()}),
        json!({"type":"response.completed", "response":{
            "status":"completed", "usage":{"input_tokens":100,"output_tokens":1}
        }}),
    ])
}

fn summary(session: &Session) -> Vec<u8> {
    tool(
        "session_history",
        json!({
            "action":"write_summary", "session_id":session.id,
            "thread_id":session.active_thread().id,
            "markdown":"# Goal / active task\nFinish the pending task."
        }),
    )
}

#[tokio::test]
async fn prolonged_provider_outage_recovers_without_replaying_completed_tools() {
    let mut replies = vec![tool(
        "bash",
        json!({"command":"printf once >> effects.txt"}),
    )];
    replies.extend((0..4).map(|_| StubHttpServer::status_response(503, "temporary outage")));
    replies.push(answer("Recovered.", 100));
    let provider = StubHttpServer::sequence(replies).await;
    let env = CliEnv::new(&provider, false);
    env.retry("max_attempts=24\ninitial_backoff_ms=1\nmax_backoff_ms=2\nmax_elapsed_ms=300000");
    let output = env.run(&["-p", "work"], b"").await;
    assert_eq!(success(&output), "Recovered.\n");
    assert_eq!(provider.connections(), 6);
    assert_eq!(
        std::fs::read_to_string(env.dir.join("effects.txt")).unwrap(),
        "once"
    );
    let diagnostics = String::from_utf8_lossy(&output.stderr);
    assert!(diagnostics.contains("retrying 5/24"), "{diagnostics}");
    assert!(diagnostics.contains("recovery budget left"));
    let saved = env.saved(&session_id(&output));
    assert!(saved.active_thread().pending_operation.is_none());
    myco::agent::validate_context(&saved.active_thread().messages).unwrap();
}

#[tokio::test]
async fn stream_overload_recovery_keeps_completed_tools_and_discards_draft_calls() {
    let provider = StubHttpServer::sequence(vec![
        tool("bash", json!({"command":"printf once >> effects.txt"})),
        StubHttpServer::sse_response(vec![
            json!({"type":"response.output_item.added", "output_index":0,
                "item":{"type":"function_call", "name":"bash", "arguments":""}}),
            json!({"type":"response.function_call_arguments.done", "output_index":0,
                "arguments":json!({"command":"printf UNVALIDATED >> effects.txt"}).to_string()}),
            json!({"type":"error", "code":"server_error", "message":"controlled overload"}),
        ]),
        answer("Recovered.", 100),
    ])
    .await;
    let env = CliEnv::new(&provider, false);
    env.retry("initial_backoff_ms=1\nmax_elapsed_ms=300000");
    let output = env.run(&["-p", "work"], b"").await;
    assert_eq!(success(&output), "Recovered.\n");
    assert_eq!(provider.connections(), 3);
    assert_eq!(
        std::fs::read_to_string(env.dir.join("effects.txt")).unwrap(),
        "once"
    );
    let diagnostics = String::from_utf8_lossy(&output.stderr);
    assert!(diagnostics.contains("server_error") && diagnostics.contains("retrying 2/3"));
    let saved = env.saved(&session_id(&output));
    assert!(saved.active_thread().pending_operation.is_none());
    assert!(
        !serde_json::to_string(&saved)
            .unwrap()
            .contains("UNVALIDATED")
    );
    myco::agent::validate_context(&saved.active_thread().messages).unwrap();
}

#[tokio::test]
async fn recovery_deadline_aborts_a_stalled_retry_and_saves_a_settled_boundary() {
    let provider = StubHttpServer::sequence_then_pending(vec![StubHttpServer::status_response(
        503,
        "temporary outage",
    )])
    .await;
    let env = CliEnv::new(&provider, false);
    env.retry("max_attempts=24\ninitial_backoff_ms=1\nmax_elapsed_ms=150");
    let output = env.run(&["-p", "work"], b"").await;
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(provider.connections(), 2);
    assert!(String::from_utf8_lossy(&output.stderr).contains("recovery deadline exhausted"));
    assert!(output.stdout.is_empty());
    let saved = env.saved(&session_id(&output));
    assert!(saved.active_thread().pending_operation.is_none());
    myco::agent::validate_context(&saved.active_thread().messages).unwrap();
}

#[tokio::test]
async fn cancelling_a_visible_prolonged_recovery_wait_does_not_send_another_request() {
    let provider = StubHttpServer::sequence(vec![StubHttpServer::status_response(
        503,
        "temporary outage",
    )])
    .await;
    let env = CliEnv::new(&provider, false);
    let session = env.seed();
    env.retry("max_attempts=24\ninitial_backoff_ms=30000\nmax_elapsed_ms=300000");
    let mut child = env
        .command(&["-p", "work", "--resume", &session.id])
        .spawn()
        .unwrap();
    drop(child.stdin.take());
    let mut stderr = tokio::io::BufReader::new(child.stderr.take().unwrap());
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let mut line = String::new();
            assert!(stderr.read_line(&mut line).await.unwrap() > 0);
            if line.contains("recovery budget left") {
                assert!(line.contains("retrying 2/24 in 30.0s"), "{line}");
                break;
            }
        }
    })
    .await
    .unwrap();
    child.stderr = Some(stderr.into_inner());
    interrupt(&child);
    let output = wait(child).await;
    assert_eq!(output.status.code(), Some(130), "{output:?}");
    assert_eq!(provider.connections(), 1);
    assert!(
        env.saved(&session.id)
            .active_thread()
            .pending_operation
            .is_none()
    );
}

#[tokio::test]
async fn print_auto_continues_until_the_agent_disables_and_persists_the_setting() {
    let provider = StubHttpServer::sequence(vec![
        answer("More work remains.", 100),
        tool("session_meta", json!({"action":"disable_auto_continue"})),
        answer("Complete.", 100),
    ])
    .await;
    let env = CliEnv::new(&provider, false);
    let output = env
        .run(&["-p", "finish the task", "--auto-continue"], b"")
        .await;
    assert_eq!(success(&output), "More work remains.Complete.\n");
    assert_eq!(provider.connections(), 3);
    let saved = env.saved(&session_id(&output));
    assert!(!saved.auto_continue);
    assert_eq!(saved.active_thread().user_turn_timestamps.len(), 1);
}

#[tokio::test]
async fn print_auto_retries_transient_and_terminal_errors_without_reaccepting_input() {
    let provider = StubHttpServer::sequence(vec![
        StubHttpServer::status_response(503, r#"{"error":"temporarily unavailable"}"#),
        StubHttpServer::status_response(400, r#"{"error":"request rejected"}"#),
        tool("session_meta", json!({"action":"disable_auto_continue"})),
        answer("Recovered.", 100),
    ])
    .await;
    let env = CliEnv::new(&provider, false);
    let output = env
        .run(&["-p", "finish the task", "--auto-continue"], b"")
        .await;
    assert_eq!(success(&output), "Recovered.\n");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("auto-continue retrying in 1.0s"),
        "{stderr}"
    );
    assert!(
        stderr.contains("auto-continue retrying in 2.0s"),
        "{stderr}"
    );
    assert!(stderr.contains("temporarily unavailable"), "{stderr}");
    assert!(stderr.contains("request rejected"), "{stderr}");
    assert_eq!(provider.connections(), 4);
    let saved = env.saved(&session_id(&output));
    assert!(!saved.auto_continue);
    assert_eq!(saved.active_thread().user_turn_timestamps.len(), 1);
}

#[tokio::test]
async fn print_auto_retry_wait_can_be_interrupted_without_another_attempt() {
    let provider = StubHttpServer::sequence(vec![
        StubHttpServer::status_response(400, r#"{"error":"request rejected"}"#),
        answer("Must not retry after cancellation.", 100),
    ])
    .await;
    let env = CliEnv::new(&provider, false);
    let session = env.seed();
    let mut child = env
        .command(&[
            "-p",
            "finish the task",
            "--auto-continue",
            "--resume",
            &session.id,
        ])
        .spawn()
        .unwrap();
    drop(child.stdin.take());
    let mut stderr = BufReader::new(child.stderr.take().unwrap()).lines();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(line) = stderr.next_line().await.unwrap() {
            if line.contains("auto-continue retrying") {
                return;
            }
        }
        panic!("CLI stopped without announcing the retry");
    })
    .await
    .unwrap();
    let accepted = env
        .saved(&session.id)
        .active_thread()
        .user_turn_timestamps
        .clone();
    interrupt(&child);
    let output = tokio::time::timeout(Duration::from_secs(2), wait(child))
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(130), "{output:?}");
    assert!(output.stdout.is_empty());
    assert_eq!(provider.connections(), 1);
    assert_eq!(accepted.len(), 1);
    assert_eq!(
        env.saved(&session.id).active_thread().user_turn_timestamps,
        accepted
    );
}

#[tokio::test]
async fn terminal_auto_continue_command_persists_without_starting_model_work() {
    let provider = StubHttpServer::sequence(vec![]).await;
    let env = CliEnv::new(&provider, false);
    let output = env
        .run(
            &["--mode", "cli"],
            b"/auto-continue on\n/auto-continue\n/quit\n",
        )
        .await;
    success(&output);
    let id = session_id(&output);
    assert!(env.saved(&id).auto_continue);
    assert_eq!(provider.connections(), 0);
    let output = env
        .run(
            &["--mode", "cli", "--resume", &id, "--auto-continue=false"],
            b"/quit\n",
        )
        .await;
    success(&output);
    assert!(!env.saved(&id).auto_continue);
}

#[tokio::test]
async fn print_accepts_an_argument_stdin_or_both_and_keeps_stdout_clean() {
    for (args, input, expected) in [
        (vec!["-p", "hello"], "", "hello"),
        (vec!["-p"], "piped @missing.png\n", "piped @missing.png\n"),
        (
            vec!["-p", "review"],
            "diff @missing.png\n",
            "diff @missing.png\n\nreview",
        ),
    ] {
        let provider = StubHttpServer::sequence(vec![answer("Hello!", 100)]).await;
        let env = CliEnv::new(&provider, false);
        let output = env.run(&args, input.as_bytes()).await;
        assert_eq!(success(&output), "Hello!\n");
        let saved = env.saved(&session_id(&output));
        assert!(serde_json::to_string(&saved).unwrap().contains("Hello!"));
        let request = provider.captured().await.body;
        let input = request["input"][0]["content"].as_str().unwrap();
        assert!(
            input.contains(expected),
            "missing {expected:?} in {input:?}"
        );
    }
}

#[tokio::test]
async fn invalid_input_fails_before_a_model_call_and_provider_errors_are_nonzero() {
    let provider = StubHttpServer::sequence(vec![StubHttpServer::status_response(
        400,
        r#"{"error":{"message":"scripted rejection"}}"#,
    )])
    .await;
    let env = CliEnv::new(&provider, false);
    for (args, input) in [
        (vec!["-p"], b" \n".as_slice()),
        (vec!["-p", "@missing.png"], b"".as_slice()),
    ] {
        let output = env.run(&args, input).await;
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(output.stdout.is_empty());
        assert_eq!(provider.connections(), 0);
    }
    let output = env.run(&["-p", "hello"], b"").await;
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("scripted rejection"));
    assert!(env.session_path(&session_id(&output)).exists());
}

#[tokio::test]
async fn gateway_cap_compacts_accumulated_tool_images_before_upload_and_continues() {
    let provider = StubHttpServer::sequence(vec![]).await;
    let env = CliEnv::new(&provider, false);
    let session = env.seed();
    let provider = StubHttpServer::sequence(vec![
        answer("Earlier answer.", 100),
        tool("view_image", json!({"path":"first.png"})),
        tool("view_image", json!({"path":"second.png"})),
        summary(&session),
        answer("private compactor answer", 100),
        answer("Recovered.", 100),
        answer("Resumed.", 100),
    ])
    .await;
    std::fs::write(&env.config, format!(
        "model = \"pipetest\"\n[gateways.test]\nprotocol = \"openai-responses\"\nbase_url = {:?}\nmax_request_bytes = 200_000\n[models.pipetest]\ngateway = \"test\"\ncontext_window = 100000\n",
        provider.base_url(),
    )).unwrap();
    // Each image fits separately; retaining both exceeds the whole-request cap.
    let mut image = vec![0; 90_000];
    image[..4].copy_from_slice(b"\x89PNG");
    for name in ["first.png", "second.png"] {
        std::fs::write(env.dir.join(name), &image).unwrap();
    }
    let first = env
        .run(&["-p", "remember this", "--resume", &session.id], b"")
        .await;
    assert_eq!(success(&first), "Earlier answer.\n");
    let id = session_id(&first);
    let recovered = env
        .run(&["-p", "inspect the images", "--resume", &id], b"")
        .await;
    assert_eq!(success(&recovered), "Recovered.\n");
    assert!(
        String::from_utf8_lossy(&recovered.stderr).contains("compaction complete"),
        "{recovered:?}"
    );
    assert_eq!(
        provider.connections(),
        6,
        "the oversized request must be replaced by compaction and continuation"
    );
    let saved = env.saved(&id);
    assert_eq!(saved.threads().len(), 2);
    let active = serde_json::to_string(&saved.active_thread().messages).unwrap();
    assert!(active.contains("Finish the pending task."));
    assert!(active.len() < 64 * 1024);
    assert!(!active.contains("myco-image:sha256:"));
    let original = serde_json::to_string(&saved.threads()[0]).unwrap();
    assert!(original.contains("Earlier answer."));
    assert!(original.contains("inspect the images"));
    assert_eq!(
        original.matches("myco-image:sha256:").count(),
        2,
        "the predecessor must preserve both original images"
    );
    let resumed = env
        .run(&["-p", "continue without images", "--resume", &id], b"")
        .await;
    assert_eq!(success(&resumed), "Resumed.\n");
    assert_eq!(provider.connections(), 7);
}

#[tokio::test]
async fn print_runs_tools_once_and_resumes_the_saved_session_by_prefix() {
    let provider = StubHttpServer::sequence(vec![
        tool(
            "bash",
            json!({"command":"printf x >> marker; printf tool-output"}),
        ),
        answer("Done.", 100),
        answer("Resumed.", 100),
    ])
    .await;
    let env = CliEnv::new(&provider, false);
    let first = env.run(&["-p", "do work"], b"").await;
    assert_eq!(success(&first), "Done.\n");
    let id = session_id(&first);
    assert_eq!(
        std::fs::read_to_string(env.dir.join("marker")).unwrap(),
        "x"
    );
    let second = env
        .run(&["-p", "continue", "--resume", &id[..12]], b"")
        .await;
    assert_eq!(success(&second), "Resumed.\n");
    assert_eq!(session_id(&second), id);
    let saved = env.saved(&id);
    assert_eq!(saved.active_thread().user_turn_timestamps.len(), 2);
    assert!(
        serde_json::to_string(&saved)
            .unwrap()
            .contains("tool-output")
    );
    assert_eq!(
        std::fs::read_to_string(env.dir.join("marker")).unwrap(),
        "x"
    );
    assert_eq!(provider.connections(), 3);
}

#[tokio::test]
async fn terminal_chat_recovers_after_an_error_and_shows_actual_tool_status() {
    let provider = StubHttpServer::sequence(vec![
        StubHttpServer::status_response(400, r#"{"error":{"message":"scripted rejection"}}"#),
        tool("bash", json!({"command":"exit 7", "host":"local"})),
        answer("The command failed.", 100),
        answer("Next turn.", 100),
    ])
    .await;
    let env = CliEnv::new(&provider, false);
    let output = env
        .run(
            &["--mode", "cli"],
            b"first\nsecond\nthird\n/session\n/quit\n",
        )
        .await;
    assert_eq!(success(&output), "The command failed.\nNext turn.\n");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("scripted rejection"), "{stderr}");
    assert!(stderr.contains("command\nexit 7\nhost\nlocal"), "{stderr}");
    assert!(stderr.contains("bash · exit 7"), "{stderr}");
    assert_eq!(
        env.saved(&session_id(&output))
            .active_thread()
            .user_turn_timestamps
            .len(),
        3
    );
    assert_eq!(provider.connections(), 4);
}

#[tokio::test]
async fn print_auto_compacts_and_continues_without_printing_the_compactors_answer() {
    let provider = StubHttpServer::sequence(vec![]).await;
    let env = CliEnv::new(&provider, true);
    let session = env.seed();
    let provider = StubHttpServer::sequence(vec![
        answer("Working.\n", 80_000),
        summary(&session),
        answer("private compactor answer", 100),
        answer("Finished.", 100),
    ])
    .await;
    env.use_provider(&provider);
    let output = env.run(&["-p", "task", "--resume", &session.id], b"").await;
    assert_eq!(success(&output), "Working.\nFinished.\n");
    assert_eq!(provider.connections(), 4);
    assert_eq!(env.saved(&session.id).threads().len(), 2);
}

#[tokio::test]
async fn compaction_rewrites_identical_summary_and_retries_without_repeating_the_write() {
    let provider = StubHttpServer::sequence(vec![]).await;
    let env = CliEnv::new(&provider, true);
    let session = env.seed();
    // A failed prior attempt may have left the same summary on disk.
    let summary_path = env
        .session_path(&session.id)
        .with_extension(format!("{}.summary.md", session.active_thread().id));
    std::fs::write(
        summary_path,
        "# Goal / active task\nFinish the pending task.",
    )
    .unwrap();
    let provider = StubHttpServer::sequence(vec![
        answer("Working.\n", 80_000),
        summary(&session),
        StubHttpServer::status_response(
            503,
            r#"{"error":{"message":"temporary outage after summary write"}}"#,
        ),
        answer("private compactor answer", 100),
        answer("Finished.", 100),
    ])
    .await;
    env.use_provider(&provider);
    let config = std::fs::read_to_string(&env.config).unwrap();
    std::fs::write(
        &env.config,
        format!("{config}\n[models.pipetest.retry]\nmax_attempts = 2\ninitial_backoff_ms = 1\n"),
    )
    .unwrap();
    let output = env.run(&["-p", "task", "--resume", &session.id], b"").await;
    assert_eq!(success(&output), "Working.\nFinished.\n");
    assert_eq!(provider.connections(), 5);
    let saved = env.saved(&session.id);
    assert_eq!(saved.threads().len(), 2);
    assert_eq!(saved.threads()[0].id, session.active_thread().id);
    assert!(
        serde_json::to_string(&saved.threads()[0])
            .unwrap()
            .contains("Working.")
    );
    for thread in saved.threads() {
        myco::agent::validate_context(&thread.messages).unwrap();
        assert!(thread.pending_operation.is_none());
    }
}

#[tokio::test]
async fn compact_worker_authentication_errors_and_request_limits_stop_without_more_requests() {
    for limited in [false, true] {
        let provider = StubHttpServer::sequence(vec![]).await;
        let env = CliEnv::new(&provider, false);
        let session = env.seed();
        let provider = StubHttpServer::sequence(vec![
            answer("Initial answer.", 100),
            summary(&session),
            StubHttpServer::status_response(
                if limited { 503 } else { 401 },
                r#"{"error":{"message":"scripted rejection"}}"#,
            ),
            answer("This request must not happen.", 100),
        ])
        .await;
        env.use_provider(&provider);
        let original = std::fs::read_to_string(&env.config).unwrap();
        let limit = if limited {
            "compaction_max_requests = 2\n"
        } else {
            ""
        };
        std::fs::write(&env.config, format!("{limit}{original}\n[models.pipetest.retry]\nmax_attempts = 3\ninitial_backoff_ms = 1\n")).unwrap();
        let output = env
            .run(
                &["--mode", "cli", "--resume", &session.id],
                b"task\n/compact\n/quit\n",
            )
            .await;
        assert_eq!(success(&output), "Initial answer.\n");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(if limited {
                "2-request limit"
            } else {
                "scripted rejection"
            }),
            "{stderr}"
        );
        assert_eq!(provider.connections(), 3);
        let saved = env.saved(&session.id);
        assert_eq!(saved.threads().len(), 1);
        assert!(saved.active_thread().pending_operation.is_none());
        myco::agent::validate_context(&saved.active_thread().messages).unwrap();
    }
}

async fn wait_for_file(path: &Path) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("tool must start");
}

fn interrupt(child: &Child) {
    // SAFETY: the child is still owned and has not been reaped.
    assert_eq!(
        unsafe { libc::kill(child.id().unwrap() as libc::pid_t, libc::SIGINT) },
        0
    );
}

#[tokio::test]
async fn cancel_settles_tool_results_and_refuses_a_concurrent_writer() {
    let provider = StubHttpServer::sequence(vec![tool(
        "bash",
        json!({
            "command":"touch started; sleep 30; touch should-not-exist"
        }),
    )])
    .await;
    let env = CliEnv::new(&provider, false);
    let session = env.seed();
    let mut child = env
        .command(&["-p", "work", "--resume", &session.id])
        .spawn()
        .unwrap();
    drop(child.stdin.take());
    wait_for_file(&env.dir.join("started")).await;
    let contender = env
        .run(&["-p", "interfere", "--resume", &session.id], b"")
        .await;
    assert_eq!(contender.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&contender.stderr).contains("already open"),
        "{contender:?}"
    );
    interrupt(&child);
    let output = wait(child).await;
    assert_eq!(output.status.code(), Some(130), "{output:?}");
    let saved = env.saved(&session.id);
    myco::agent::validate_context(&saved.active_thread().messages).unwrap();
    assert!(saved.active_thread().pending_operation.is_none());
    assert!(!env.dir.join("should-not-exist").exists());
    assert_eq!(provider.connections(), 1);
}

#[tokio::test]
async fn cancelling_auto_compaction_exits_without_a_continuation() {
    let provider = StubHttpServer::sequence_then_pending(vec![answer("Working.", 80_000)]).await;
    let env = CliEnv::new(&provider, true);
    let mut child = env.command(&["-p", "work"]).spawn().unwrap();
    drop(child.stdin.take());
    tokio::time::timeout(Duration::from_secs(10), async {
        while provider.connections() < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    interrupt(&child);
    let output = wait(child).await;
    assert_eq!(output.status.code(), Some(130), "{output:?}");
    assert_eq!(env.saved(&session_id(&output)).threads().len(), 1);
    assert_eq!(provider.connections(), 2);
}

#[tokio::test]
async fn terminal_manual_compaction_returns_to_the_prompt_without_continuing() {
    let initial = StubHttpServer::sequence(vec![]).await;
    let env = CliEnv::new(&initial, true);
    let session = env.seed();
    let provider = StubHttpServer::sequence(vec![
        answer("Done.", 100),
        summary(&session),
        answer("Summary ready.", 100),
    ])
    .await;
    env.use_provider(&provider);
    let output = env
        .run(
            &["--mode", "cli", "--resume", &session.id],
            b"task\n/compact\n/quit\n",
        )
        .await;
    assert_eq!(success(&output), "Done.\n");
    assert!(String::from_utf8_lossy(&output.stderr).contains("compacted into thread"));
    assert_eq!(env.saved(&session.id).threads().len(), 2);
    assert_eq!(provider.connections(), 3);
}

#[tokio::test]
async fn resume_uses_the_saved_model_unless_explicitly_overridden() {
    let provider = StubHttpServer::sequence(vec![
        answer("One.", 100),
        answer("Two.", 100),
        answer("Three.", 100),
        answer("Four.", 100),
    ])
    .await;
    let env = CliEnv::new(&provider, false);
    let config = std::fs::read_to_string(&env.config).unwrap();
    let model = config.split_once("[models.pipetest]").unwrap().1;
    std::fs::write(&env.config, format!("{config}\n[models.selected]\n{model}")).unwrap();
    let first = env.run(&["-p", "first", "--model", "selected"], b"").await;
    success(&first);
    let id = session_id(&first);
    assert_eq!(env.saved(&id).model, "selected");
    success(&env.run(&["-p", "next", "--resume", &id], b"").await);
    assert_eq!(env.saved(&id).model, "selected");
    success(
        &env.run(&["-p", "next", "--resume", &id, "--model", "pipetest"], b"")
            .await,
    );
    let saved = env.saved(&id);
    assert_eq!(
        myco::RuntimeRecord::latest(&saved.active_thread().messages)
            .unwrap()
            .model
            .key,
        "pipetest"
    );
    success(&env.run(&["-p", "next", "--resume", &id], b"").await);
    let saved = env.saved(&id);
    assert_eq!(
        myco::RuntimeRecord::latest(&saved.active_thread().messages)
            .unwrap()
            .model
            .key,
        "pipetest"
    );
}

#[tokio::test]
async fn exiting_print_mode_stops_retained_shell_descendants() {
    for fails in [false, true] {
        let response = if fails {
            StubHttpServer::status_response(400, r#"{"error":{"message":"stop"}}"#)
        } else {
            answer("Started.", 100)
        };
        let provider = StubHttpServer::sequence(vec![
            tool(
                "bash",
                json!({"action":"start", "session_id":"owned", "idle_ms":10,
                "command":"(sleep 2; touch orphaned) & wait"}),
            ),
            response,
        ])
        .await;
        let env = CliEnv::new(&provider, false);
        let output = env.run(&["-p", "start work"], b"").await;
        assert_eq!(output.status.code(), Some(i32::from(fails)), "{output:?}");
        tokio::time::sleep(Duration::from_millis(2300)).await;
        assert!(
            !env.dir.join("orphaned").exists(),
            "a CLI-owned descendant survived exit"
        );
    }
}

#[tokio::test]
async fn a_closed_output_pipe_cancels_cleanly_instead_of_panicking() {
    let provider = StubHttpServer::sequence(vec![answer("Answer.", 100)]).await;
    let env = CliEnv::new(&provider, false);
    let mut child = env.command(&["-p", "task"]).spawn().unwrap();
    drop(child.stdin.take());
    drop(child.stdout.take());
    let output = wait(child).await;
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("write answer"));
    let saved = env.saved(&session_id(&output));
    assert!(saved.active_thread().pending_operation.is_none());
    myco::agent::validate_context(&saved.active_thread().messages).unwrap();
}

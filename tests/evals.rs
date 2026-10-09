//! Evals use real CLI processes and local provider stubs; no model credentials.
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::Duration;

mod test_utils;
use test_utils::StubHttpServer;

struct Fixture {
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("myco-eval-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("fixture")).unwrap();
        std::fs::write(root.join("fixture/input.txt"), "task input").unwrap();
        std::fs::write(
            root.join("task.txt"),
            "Read input.txt and write result.txt containing done.",
        )
        .unwrap();
        std::fs::write(root.join("grader.py"), "import json\nfrom pathlib import Path\np = Path('result.txt')\nprint(json.dumps({'score': float(p.is_file() and p.read_text() == 'done'), 'feedback': 'result.txt must contain done'}))\n").unwrap();
        Self { root }
    }
    async fn cli(&self, args: Vec<String>) -> std::process::Output {
        tokio::time::timeout(
            Duration::from_secs(25),
            tokio::process::Command::new(env!("CARGO_BIN_EXE_myco-eval"))
                .args(args)
                .env("MYCO_HOME", self.root.join("source-home"))
                .env("MYCO_PROFILE", "default")
                .env_remove("MYCO_CONFIG")
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap()
    }
    async fn create(&self) {
        let output = self
            .cli(vec![
                "create".into(),
                self.path("case"),
                "--task-file".into(),
                self.path("task.txt"),
                "--workspace".into(),
                self.path("fixture"),
                "--grader".into(),
                self.path("grader.py"),
            ])
            .await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    fn path(&self, relative: &str) -> String {
        self.root.join(relative).to_string_lossy().into_owned()
    }
    fn configure(&self, server: &StubHttpServer) {
        std::fs::write(self.root.join("config.toml"), format!("model = \"test\"\n[models.test]\nprotocol = \"openai-responses\"\nbase_url = {:?}\nauth = {{ source = \"none\" }}\ncontext_window = 100000\n[models.test.retry]\nmax_attempts = 1\n", server.base_url())).unwrap();
    }
    async fn run(&self, extra: &[&str]) -> std::process::Output {
        let mut args = vec![
            "run".into(),
            self.path("case"),
            "--config".into(),
            self.path("config.toml"),
            "--model".into(),
            "test".into(),
            "--output".into(),
            self.path("runs"),
        ];
        if !extra.contains(&"--timeout-secs") {
            args.extend(["--timeout-secs".into(), "5".into()]);
        }
        args.extend(extra.iter().map(|value| (*value).into()));
        self.cli(args).await
    }
    fn results(&self) -> Vec<Value> {
        let mut results = vec![];
        for entry in std::fs::read_dir(self.root.join("runs")).unwrap() {
            let path = entry.unwrap().path().join("result.json");
            if path.is_file() {
                results.push(serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap());
            }
        }
        results
    }
    fn run_path(&self) -> PathBuf {
        std::fs::read_dir(self.root.join("runs"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.join("result.json").is_file())
            .unwrap()
    }
    fn events(&self) -> Vec<Value> {
        std::fs::read_to_string(self.run_path().join("events.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn tool(command: &str, input: u64, output: u64) -> Vec<u8> {
    StubHttpServer::sse_response(vec![
        json!({"type":"response.output_item.added", "output_index":0, "item":{"type":"function_call", "name":"bash", "call_id":"call", "arguments":""}}),
        json!({"type":"response.function_call_arguments.done", "output_index":0, "arguments":json!({"command":command}).to_string()}),
        json!({"type":"response.completed", "response":{"status":"completed", "usage":{"input_tokens":input,"output_tokens":output}}}),
    ])
}
fn answer(input: u64, output: u64) -> Vec<u8> {
    StubHttpServer::sse_response(vec![
        json!({"type":"response.output_text.delta", "output_index":0, "content_index":0, "delta":"Done."}),
        json!({"type":"response.completed", "response":{"status":"completed", "usage":{"input_tokens":input,"output_tokens":output}}}),
    ])
}
fn report(output: &std::process::Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(&output.stdout)))
}

fn git(path: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(["-c", "init.templateDir=", "-C"])
        .arg(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

#[tokio::test]
async fn git_run_replays_after_moving_artifacts_and_deleting_original_sources() {
    let fixture = Fixture::new();
    let source = fixture.root.join("fixture");
    git(&source, &["init", "--quiet"]);
    git(&source, &["add", "input.txt"]);
    git(
        &source,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "Pinned input",
        ],
    );
    let revision = git(&source, &["rev-parse", "HEAD"]);
    git(
        &source,
        &[
            "config",
            "http.extraHeader",
            "Authorization: LOCAL_CONFIG_SECRET",
        ],
    );
    git(
        &source,
        &[
            "config",
            "remote.private.url",
            "https://user:LOCAL_URL_SECRET@example.invalid/private",
        ],
    );
    // The snapshot includes only ancestry of the pinned commit, not later refs.
    std::fs::write(source.join("unrelated.txt"), "UNRELATED_COMMIT_SECRET").unwrap();
    git(&source, &["add", "unrelated.txt"]);
    git(
        &source,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "-m",
            "Unrelated later commit",
        ],
    );
    let later = git(&source, &["rev-parse", "HEAD"]);
    let created = fixture
        .cli(vec![
            "create".into(),
            fixture.path("case"),
            "--task-file".into(),
            fixture.path("task.txt"),
            "--repo".into(),
            fixture.path("fixture"),
            "--revision".into(),
            revision.clone(),
            "--grader".into(),
            fixture.path("grader.py"),
        ])
        .await;
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let mut replies = vec![];
    for _ in 0..2 {
        replies.extend([
            tool(
                "test -f input.txt && test ! -e unrelated.txt && printf done > result.txt",
                100,
                5,
            ),
            answer(70, 7),
        ]);
    }
    let server = StubHttpServer::sequence(replies).await;
    fixture.configure(&server);
    assert_eq!(
        report(&fixture.run(&[]).await)["groups"][0]["success_rate"],
        1.0
    );

    let moved = fixture.root.join("moved-run");
    std::fs::rename(fixture.run_path(), &moved).unwrap();
    std::fs::remove_dir_all(&source).unwrap();
    std::fs::remove_dir_all(fixture.root.join("case")).unwrap();
    std::fs::remove_file(fixture.root.join("config.toml")).unwrap();
    let workspace = moved.join("workspace");
    assert!(!workspace.join(".git/objects/info/alternates").exists());
    assert_eq!(git(&workspace, &["rev-parse", "HEAD"]), revision);
    git(&workspace, &["fsck", "--full"]);
    assert!(!git(&workspace, &["rev-list", "--all"]).contains(&later));
    let local_config = std::fs::read_to_string(workspace.join(".git/config")).unwrap();
    assert!(!local_config.contains("SECRET"));
    let frozen: Value =
        serde_json::from_slice(&std::fs::read(moved.join("case/case.json")).unwrap()).unwrap();
    assert_eq!(frozen["workspace"]["repo"], "source.bundle");
    let provenance: Value =
        serde_json::from_slice(&std::fs::read(moved.join("provenance.json")).unwrap()).unwrap();
    assert_ne!(provenance["case_hash"], provenance["frozen_case_hash"]);
    myco::eval::load_case(&moved.join("case")).unwrap();

    // Replay supplies a new config; the original auth/environment is not bundled.
    fixture.configure(&server);
    let replay = fixture
        .cli(vec![
            "run".into(),
            moved.join("case").to_string_lossy().into_owned(),
            "--config".into(),
            fixture.path("config.toml"),
            "--model".into(),
            "test".into(),
            "--output".into(),
            fixture.path("replayed"),
            "--timeout-secs".into(),
            "5".into(),
        ])
        .await;
    assert_eq!(report(&replay)["groups"][0]["success_rate"], 1.0);
    assert_eq!(server.connections(), 4);
}

#[tokio::test]
async fn repeated_cases_use_fresh_workspaces_count_all_requests_and_resume_finished_results() {
    let fixture = Fixture::new();
    fixture.create().await;
    let mut replies = vec![];
    for _ in 0..2 {
        replies.extend([
            tool(
                "test ! -e result.txt && test -f input.txt && printf done > result.txt",
                100,
                5,
            ),
            answer(70, 7),
        ]);
    }
    let server = StubHttpServer::sequence(replies).await;
    fixture.configure(&server);
    let output = report(&fixture.run(&["--repeat", "2"]).await);
    assert_eq!(output["groups"][0]["success_rate"], 1.0, "{output}");
    assert_eq!(output["groups"][0]["requests"], 4);
    assert_eq!(output["groups"][0]["input_tokens"], 340);
    assert_eq!(output["groups"][0]["output_tokens"], 24);
    assert!(output["groups"][0]["estimated_cost_usd"].is_null());
    assert!(!fixture.root.join("fixture/result.txt").exists());
    let results = fixture.results();
    assert_eq!(results.len(), 2);
    assert_ne!(
        results[0]["agent"]["session_id"],
        results[1]["agent"]["session_id"]
    );
    let again = report(&fixture.run(&["--repeat", "2"]).await);
    assert_eq!(output, again);
    assert_eq!(server.connections(), 4);
    std::fs::write(
        fixture.root.join("prices.json"),
        r#"{"test":{"input_per_million":1,"cached_input_per_million":0.5,"output_per_million":2}}"#,
    )
    .unwrap();
    let cost = report(
        &fixture
            .cli(vec![
                "report".into(),
                fixture.path("runs"),
                "--prices".into(),
                fixture.path("prices.json"),
                "--min-success-rate".into(),
                "1".into(),
            ])
            .await,
    );
    assert!((cost["groups"][0]["estimated_cost_usd"].as_f64().unwrap() - 0.000388).abs() < 1e-9);
}

#[tokio::test]
async fn model_aliases_get_distinct_runs_and_effort_changes_do_not_reuse_scores() {
    let fixture = Fixture::new();
    fixture.create().await;
    let server = StubHttpServer::sequence(vec![answer(10, 1); 4]).await;
    fixture.configure(&server);
    let path = fixture.root.join("config.toml");
    let original = std::fs::read_to_string(&path)
        .unwrap()
        .replace("[models.test]\n", "[models.test]\napi_id = \"shared\"\n");
    let alias =
        original[original.find("[models.test]").unwrap()..].replace("models.test", "models.alias");
    std::fs::write(path, format!("{original}\n{alias}")).unwrap();
    let first = report(&fixture.run(&["--model", "alias", "--jobs", "2"]).await);
    assert_eq!(first["groups"].as_array().unwrap().len(), 2);
    assert_eq!(fixture.results().len(), 2);
    assert_eq!(server.connections(), 2);
    let changed = report(&fixture.run(&["--model", "alias", "--effort", "low"]).await);
    assert_eq!(changed["groups"].as_array().unwrap().len(), 4);
    assert_eq!(fixture.results().len(), 4);
    assert_eq!(server.connections(), 4);
}

#[tokio::test]
async fn runtime_policy_changes_invalidate_cached_results_and_record_effective_settings() {
    let fixture = Fixture::new();
    fixture.create().await;
    let server = StubHttpServer::sequence(vec![answer(10, 1); 3]).await;
    fixture.configure(&server);
    let path = fixture.root.join("config.toml");
    let original = std::fs::read_to_string(&path).unwrap().replace(
        "auth = { source = \"none\" }",
        "auth = \"fixture-auth-secret\"",
    );
    for settings in [
        "",
        "compaction_max_requests = 2\n",
        "compaction_max_requests = 2\nmax_prelude_bytes = 10000\n",
    ] {
        std::fs::write(&path, format!("{settings}{original}")).unwrap();
        report(&fixture.run(&[]).await);
    }
    assert_eq!(fixture.results().len(), 3);
    assert_eq!(server.connections(), 3);
    report(&fixture.run(&[]).await);
    assert_eq!(server.connections(), 3);
    let provenance = std::fs::read_to_string(fixture.run_path().join("provenance.json")).unwrap();
    assert!(!provenance.contains("fixture-auth-secret"));
    let provenance: Value = serde_json::from_str(&provenance).unwrap();
    assert_eq!(provenance["version"], 1);
    assert_eq!(provenance["configuration"]["backend"]["effort"], "high");
    assert_eq!(
        provenance["configuration"]["model"]["context_window_tokens"],
        100000
    );
    assert!(provenance["configuration"]["runtime"]["compaction_max_requests"].is_u64());
    assert!(
        provenance["configuration"]["system_prompt"]
            .as_str()
            .unwrap()
            .contains("current task workspace")
    );
    assert!(
        provenance["configuration"]["tools"]
            .as_array()
            .unwrap()
            .len()
            > 1
    );
    assert_eq!(provenance["artifacts"]["trace"], "events.jsonl");
    assert!(
        chrono::DateTime::parse_from_rfc3339(provenance["created_at"].as_str().unwrap()).is_ok()
    );
}

#[tokio::test]
async fn transient_and_broken_stream_recovery_retains_attempts_without_replaying_tools() {
    let fixture = Fixture::new();
    fixture.create().await;
    let partial = json!({"type":"response.output_text.delta", "output_index":0, "content_index":0, "delta":"Abandoned draft"});
    let body = format!("data: {partial}\n\n");
    let broken = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len() + 100).into_bytes();
    let server = StubHttpServer::sequence(vec![
        tool(
            "printf done > result.txt; printf once >> effects.txt",
            100,
            5,
        ),
        StubHttpServer::status_response(503, r#"{"error":{"message":"fixture outage"}}"#),
        broken,
        answer(70, 7),
    ])
    .await;
    fixture.configure(&server);
    let config = fixture.root.join("config.toml");
    std::fs::write(
        &config,
        std::fs::read_to_string(&config).unwrap().replace(
            "max_attempts = 1",
            "max_attempts = 3\ninitial_backoff_ms = 1",
        ),
    )
    .unwrap();
    let result = report(&fixture.run(&[]).await);
    assert_eq!(
        result["groups"][0]["success_rate"],
        1.0,
        "{:?}",
        fixture.results()
    );
    assert_eq!(result["groups"][0]["requests"], 4);
    assert_eq!(result["groups"][0]["requests_without_usage"], 2);
    assert_eq!(server.connections(), 4);
    assert_eq!(
        std::fs::read_to_string(fixture.run_path().join("workspace/effects.txt")).unwrap(),
        "once"
    );
    assert_eq!(fixture.results()[0]["agent"]["answer"], "Done.");
    let events = fixture.events();
    for (index, event) in events.iter().enumerate() {
        assert_eq!(event["version"], 1);
        assert_eq!(event["sequence"], index + 1);
        assert!(chrono::DateTime::parse_from_rfc3339(event["timestamp"].as_str().unwrap()).is_ok());
    }
    assert!(
        events
            .windows(2)
            .all(|pair| pair[0]["elapsed_ms"].as_u64() <= pair[1]["elapsed_ms"].as_u64())
    );
    let requests: Vec<_> = events
        .iter()
        .filter(|event| event["event"] == "request_finished")
        .collect();
    assert_eq!(
        requests
            .iter()
            .map(|event| event["outcome"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["finished", "failed", "failed", "finished"]
    );
    for (index, event) in requests.iter().enumerate() {
        assert_eq!(event["request_id"], index + 1);
    }
    let started = events
        .iter()
        .find(|event| event["event"] == "tool_started")
        .unwrap();
    let finished = events
        .iter()
        .find(|event| event["event"] == "tool_finished")
        .unwrap();
    assert_eq!(started["call_id"], finished["call_id"]);
    assert!(started["call_id"].is_string());
    let failures: Vec<_> = events
        .iter()
        .filter(|event| event["event"] == "generation_failed")
        .collect();
    assert_eq!(failures.len(), 2);
    assert!(failures.iter().all(|event| event["retry_in_ms"].is_u64()));
}

#[tokio::test]
async fn deadline_cancels_retry_backoff_without_starting_another_request() {
    let fixture = Fixture::new();
    fixture.create().await;
    let server = StubHttpServer::sequence(vec![
        StubHttpServer::status_response(503, r#"{"error":{"message":"fixture outage"}}"#),
        answer(70, 7),
    ])
    .await;
    fixture.configure(&server);
    let config = fixture.root.join("config.toml");
    std::fs::write(
        &config,
        std::fs::read_to_string(&config).unwrap().replace(
            "max_attempts = 1",
            "max_attempts = 3\ninitial_backoff_ms = 30000",
        ),
    )
    .unwrap();
    report(&fixture.run(&["--timeout-secs", "1"]).await);
    assert_eq!(fixture.results()[0]["status"], "timeout");
    assert_eq!(server.connections(), 1);
    let events = fixture.events();
    let failure = events
        .iter()
        .find(|event| event["event"] == "generation_failed")
        .unwrap();
    assert_eq!(failure["retry_in_ms"], 30000);
    assert_eq!(events.last().unwrap()["event"], "run_finished");
    assert_eq!(events.last().unwrap()["status"], "timeout");
}

#[tokio::test]
async fn request_limit_and_timeout_are_recorded_without_calling_a_paid_fallback() {
    for timeout in [false, true] {
        let fixture = Fixture::new();
        fixture.create().await;
        let server = if timeout {
            StubHttpServer::sequence_then_pending(vec![]).await
        } else {
            StubHttpServer::sequence(vec![tool("printf done > result.txt", 100, 5)]).await
        };
        fixture.configure(&server);
        let args = if timeout {
            vec!["--timeout-secs", "1"]
        } else {
            vec!["--max-requests", "1"]
        };
        let mut arguments = vec![
            "run".into(),
            fixture.path("case"),
            "--config".into(),
            fixture.path("config.toml"),
            "--model".into(),
            "test".into(),
            "--output".into(),
            fixture.path("runs"),
        ];
        arguments.extend(args.into_iter().map(str::to_owned));
        let output = report(&fixture.cli(arguments).await);
        assert_eq!(output["groups"][0]["success_rate"], 0.0, "{output}");
        assert_eq!(server.connections(), 1);
        assert_eq!(
            fixture.results()[0]["status"],
            if timeout { "timeout" } else { "request_limit" }
        );
        let floor = fixture
            .cli(vec![
                "report".into(),
                fixture.path("runs"),
                "--min-success-rate".into(),
                "1".into(),
            ])
            .await;
        assert!(!floor.status.success());
    }
    let fixture = Fixture::new();
    fixture.create().await;
    let server = StubHttpServer::sequence(vec![]).await;
    fixture.configure(&server);
    let output = fixture.run(&["--free-only"]).await;
    assert!(!output.status.success());
    assert_eq!(server.connections(), 0);
}

#[tokio::test]
async fn grader_errors_are_infrastructure_failures_and_frozen_graders_cannot_be_replaced() {
    for modify in [false, true] {
        let fixture = Fixture::new();
        fixture.create().await;
        if !modify {
            std::fs::write(
                fixture.root.join("case/grader.py"),
                "raise RuntimeError('grader broken')",
            )
            .unwrap();
        }
        let command = if modify {
            "printf 'changed' > ../case/grader.py"
        } else {
            "printf done > result.txt"
        };
        let server = StubHttpServer::sequence(vec![tool(command, 100, 5), answer(70, 7)]).await;
        fixture.configure(&server);
        let output = report(&fixture.run(&[]).await);
        assert_eq!(output["groups"][0]["infrastructure_errors"], 1, "{output}");
        let result = &fixture.results()[0];
        assert_eq!(
            result["status"],
            if modify {
                "case_modified"
            } else {
                "grader_error"
            }
        );
        assert!(result["score"].is_null());
        assert!(
            !std::fs::read_to_string(fixture.root.join("case/grader.py"))
                .unwrap()
                .contains("changed")
        );
    }
}

#[tokio::test]
async fn session_export_ends_at_the_selected_task_and_copies_images_without_mutating_the_source() {
    let fixture = Fixture::new();
    let mut session = myco::Session::new("old");
    session.replace_context(
        vec![
            myco::generative_model::Message::UserMessage {
                content: vec![
                    myco::generative_model::Content::Text {
                        text: "fix the task".into(),
                    },
                    myco::generative_model::Content::Image {
                        source: "data:image/png;base64,iVBORw==".into(),
                    },
                ],
            },
            myco::generative_model::Message::AssistantMessage {
                content: vec![myco::generative_model::Content::Text {
                    text: "SECRET LATER SOLUTION".into(),
                }],
                tool_uses: vec![],
                turn_end_reason: None,
            },
        ],
        None,
    );
    let store = fixture
        .root
        .join("source-home/profiles/default/session")
        .join(&session.id[..2]);
    std::fs::create_dir_all(&store).unwrap();
    let path = store.join(format!("{}.json", session.id));
    let original = serde_json::to_vec(&session).unwrap();
    std::fs::write(&path, &original).unwrap();
    let output = fixture
        .cli(vec![
            "from-session".into(),
            session.id,
            fixture.path("case"),
            "--user-message".into(),
            "0".into(),
            "--workspace".into(),
            fixture.path("fixture"),
            "--grader".into(),
            fixture.path("grader.py"),
        ])
        .await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let context = std::fs::read_to_string(fixture.root.join("case/context.json")).unwrap();
    assert!(!context.contains("SECRET"));
    assert!(context.contains("myco-image:sha256:"));
    assert_eq!(std::fs::read(path).unwrap(), original);
    assert!(fixture.root.join("case/images").is_dir());
    assert!(
        !fixture
            .root
            .join("source-home/profiles/default/images")
            .exists()
    );
}

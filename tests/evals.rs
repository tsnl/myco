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
            "--timeout-secs".into(),
            "5".into(),
        ];
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

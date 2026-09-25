//! Exercise the actual server launcher, loopback actions, persistence, and
//! compaction without provider credentials or an external browser.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use uuid::Uuid;

mod test_utils;

struct ServerEnv {
    dir: PathBuf,
    config: PathBuf,
}

impl ServerEnv {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("myco-server-{tag}-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("config.toml");
        std::fs::write(
            &config,
            r#"model = "pipetest"
[models.pipetest]
protocol = "openai-responses"
base_url = "http://127.0.0.1:1/v1"
auth = { source = "none" }
context_window = 100000
"#,
        )
        .unwrap();
        Self { dir, config }
    }
}

impl Drop for ServerEnv {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

struct Server {
    process: tokio::process::Child,
    origin: String,
    client: reqwest::Client,
}

impl Server {
    async fn start(env: &ServerEnv, args: &[&str]) -> Self {
        let mut process = tokio::process::Command::new(env!("CARGO_BIN_EXE_myco"))
            .args(["--port", "0"])
            .args(args)
            .env("MYCO_HOME", &env.dir)
            .env("MYCO_PROFILE", "default")
            .env("MYCO_CONFIG", &env.config)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut stdout = BufReader::new(process.stdout.take().unwrap());
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(15), stdout.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let url = url::Url::parse(line.trim().strip_prefix("Browser UI: ").expect(&line)).unwrap();
        tokio::spawn(async move {
            let mut rest = String::new();
            let _ = stdout.read_to_string(&mut rest).await;
        });
        assert_eq!(url.scheme(), "http");
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        assert!(url.query().is_none(), "launch URLs need no credentials");
        Self {
            process,
            origin: url.origin().ascii_serialization(),
            client,
        }
    }

    async fn request(&self, method: &str, path: &str, body: Value) -> (u16, Value) {
        let mut request = self
            .client
            .request(method.parse().unwrap(), format!("{}{path}", self.origin));
        if !body.is_null() {
            request = request.json(&body);
        }
        let response = request.send().await.unwrap();
        let status = response.status().as_u16();
        let text = response.text().await.unwrap();
        let body = serde_json::from_str(&text).unwrap_or(Value::String(text));
        (status, body)
    }

    async fn create(&self, parent: Option<&str>, fork: bool) -> String {
        let id = Uuid::new_v4();
        let request = json!({"request_id":id, "parent_session":parent, "fork":fork});
        let (status, body) = self.request("POST", "/api/sessions", request.clone()).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["id"], id.as_simple().to_string());
        assert_eq!(self.request("POST", "/api/sessions", request).await.1, body);
        body["id"].as_str().unwrap().into()
    }

    async fn snapshot(&self, id: &str) -> Value {
        let (status, body) = self
            .request("GET", &format!("/api/sessions/{id}"), Value::Null)
            .await;
        assert_eq!(status, 200, "{body}");
        body["change"]["snapshot"].clone()
    }

    async fn action(&self, id: &str, action: Value) {
        let (status, body) = self
            .request(
                "POST",
                &format!("/api/sessions/{id}/action"),
                json!({"request_id":Uuid::new_v4(), "session_id":id, "action":action}),
            )
            .await;
        assert_eq!(status, 202, "{body}");
    }

    async fn idle(&self, id: &str) -> Value {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let snapshot = self.snapshot(id).await;
                if snapshot["busy"] == false {
                    return snapshot;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("session did not finish")
    }

    async fn submit(&self, id: &str, text: &str) -> Value {
        self.action(id, json!({"kind":"submit", "text":text})).await;
        self.idle(id).await
    }

    async fn stop(mut self) {
        // SAFETY: this is the still-owned child process, not a reused pid.
        assert_eq!(
            unsafe { libc::kill(self.process.id().unwrap() as libc::pid_t, libc::SIGINT) },
            0
        );
        let status = tokio::time::timeout(Duration::from_secs(10), self.process.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(status.success(), "{status}");
    }
}

#[tokio::test]
async fn loopback_server_needs_no_login_before_or_after_restart() {
    let env = ServerEnv::new("loopback-access");
    for _ in 0..2 {
        let server = Server::start(&env, &[]).await;
        assert!(server.origin.starts_with("http://127.0.0.1:"));
        assert!(!env.dir.join("profiles/default/tls").exists());
        assert_eq!(
            server.request("GET", "/api/sessions", Value::Null).await.0,
            200
        );
        let response = server
            .client
            .get(format!("{}/", server.origin))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert!(!response.headers().contains_key("set-cookie"));
        assert!(!response.headers().contains_key("www-authenticate"));
        assert_eq!(
            server
                .request("GET", "/auth?token=old", Value::Null)
                .await
                .0,
            404
        );
        server.stop().await;
    }
}

#[tokio::test]
async fn non_loopback_bind_addresses_fail_before_listening() {
    let env = ServerEnv::new("loopback-invalid");
    for address in ["0.0.0.0", "::", "192.168.1.10", "2001:db8::1"] {
        let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_myco"))
            .args(["--bind", address, "--port", "0"])
            .env("MYCO_HOME", &env.dir)
            .env("MYCO_PROFILE", "default")
            .output()
            .await
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("use an SSH tunnel"));
        assert!(output.stdout.is_empty());
    }
}

#[tokio::test]
async fn ipv6_loopback_serves_http_without_credentials() {
    let env = ServerEnv::new("loopback-ipv6");
    let server = Server::start(&env, &["--bind", "::1"]).await;
    assert!(server.origin.starts_with("http://[::1]:"));
    assert_eq!(
        server.request("GET", "/api/sessions", Value::Null).await.0,
        200
    );
    server.stop().await;
}

fn session_json(dir: &Path, id: &str) -> Value {
    let root = dir.join("profiles/default/session");
    let filename = format!("{}/{id}.json", &id[..2]);
    let path = if root.join(&filename).exists() {
        root.join(filename)
    } else {
        root.join("archived").join(filename)
    };
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}
fn model_answer(text: &str, input_tokens: u64) -> Vec<u8> {
    test_utils::StubHttpServer::sse_response(vec![
        serde_json::json!({"type":"response.output_text.delta", "delta":text}),
        serde_json::json!({"type":"response.completed", "response":{
            "status":"completed", "usage":{"input_tokens":input_tokens,"output_tokens":1}
        }}),
    ])
}

fn compact_test_session(env: &ServerEnv) -> myco::Session {
    let session = myco::Session::new("pipetest");
    let store = env
        .dir
        .join("profiles/default/session")
        .join(&session.id[..2]);
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(
        store.join(format!("{}.json", session.id)),
        serde_json::to_vec(&session).unwrap(),
    )
    .unwrap();
    session
}

fn write_summary_response(session: &myco::Session) -> Vec<u8> {
    use serde_json::json;
    let arguments = json!({
        "action":"write_summary", "session_id":session.id,
        "thread_id":session.active_thread().id,
        "markdown":"# Goal / active task\nFinish the pending task."
    });
    model_tool("session_history", arguments, 100)
}

fn model_tool(name: &str, arguments: serde_json::Value, input_tokens: u64) -> Vec<u8> {
    use serde_json::json;
    test_utils::StubHttpServer::sse_response(vec![
        json!({"type":"response.output_item.added", "output_index":0,
            "item":{"type":"function_call", "name":name, "call_id":"tool", "arguments":""}}),
        json!({"type":"response.function_call_arguments.done", "output_index":0, "arguments":arguments.to_string()}),
        json!({"type":"response.completed", "response":{
            "status":"completed", "usage":{"input_tokens":input_tokens,"output_tokens":1}
        }}),
    ])
}

fn configure_compact(env: &ServerEnv, server: &test_utils::StubHttpServer, enabled: bool) {
    let mut config = std::fs::read_to_string(&env.config)
        .unwrap()
        .replace("http://127.0.0.1:1/v1", &server.base_url());
    if enabled {
        config.push_str("auto_compact_at = 0.8\n");
    }
    std::fs::write(&env.config, config).unwrap();
}

#[tokio::test]
async fn server_starts_without_stdin_and_migrates_archives_before_serving() {
    let env = ServerEnv::new("archive");
    let mut saved = compact_test_session(&env);
    saved.archived = true;
    let root = env.dir.join("profiles/default/session");
    let old = root.join(&saved.id[..2]).join(format!("{}.json", saved.id));
    let bytes = serde_json::to_vec(&saved).unwrap();
    std::fs::write(&old, &bytes).unwrap();
    std::fs::write(old.with_extension("history"), "legacy input").unwrap();
    let server = Server::start(&env, &["--resume", &saved.id]).await;
    assert!(!old.exists());
    let moved = root
        .join("archived")
        .join(&saved.id[..2])
        .join(format!("{}.json", saved.id));
    assert_eq!(
        std::fs::read_to_string(moved.with_extension("history")).unwrap(),
        "legacy input"
    );
    assert_eq!(
        server.request("GET", "/api/sessions", Value::Null).await.1,
        json!([])
    );
    assert_eq!(
        server
            .request("GET", "/api/sessions?archived=true", Value::Null)
            .await
            .1[0]["id"],
        saved.id
    );
    let response = server
        .request(
            "POST",
            &format!("/api/sessions/{}/archive", saved.id),
            json!({"session_id":saved.id,"archived":false}),
        )
        .await;
    assert_eq!(response.0, 204);
    assert!(old.exists());
    assert!(!moved.exists());
    server.stop().await;
}

#[tokio::test]
async fn api_children_are_hidden_and_forks_stamp_their_own_identity_after_restart() {
    let env = ServerEnv::new("fork");
    let provider = test_utils::StubHttpServer::sequence(vec![
        model_answer("parent answer", 100),
        model_answer("child answer", 100),
    ])
    .await;
    configure_compact(&env, &provider, false);
    let server = Server::start(&env, &[]).await;
    let parent = server.create(None, false).await;
    server.submit(&parent, "parent marker").await;
    let fork = server.create(Some(&parent), true).await;
    let blank = server.create(Some(&parent), false).await;
    let listing = server.request("GET", "/api/sessions", Value::Null).await.1;
    assert_eq!(listing.as_array().unwrap().len(), 1);
    assert_eq!(listing[0]["id"], parent);
    assert_eq!(session_json(&env.dir, &fork)["kind"], "subagent");
    assert_eq!(session_json(&env.dir, &blank)["parent_session_id"], parent);
    assert!(
        !session_json(&env.dir, &blank)
            .to_string()
            .contains("parent marker")
    );
    let bad = server
        .request(
            "POST",
            "/api/sessions",
            json!({"request_id":Uuid::new_v4(), "fork":true}),
        )
        .await;
    assert_eq!(bad.0, 400);
    assert_eq!(
        server
            .request(
                "POST",
                &format!("/api/sessions/{blank}/archive"),
                json!({"session_id":blank,"archived":true})
            )
            .await
            .0,
        204
    );
    assert_eq!(
        server
            .request("GET", "/api/sessions?archived=true", Value::Null)
            .await
            .1,
        json!([])
    );
    server.stop().await;

    let server = Server::start(&env, &["--resume", &fork]).await;
    let retried = server
        .request(
            "POST",
            "/api/sessions",
            json!({
                "request_id":Uuid::parse_str(&fork).unwrap(), "parent_session":parent, "fork":true,
            }),
        )
        .await;
    assert_eq!(retried, (200, json!({"id":fork})));
    let output = server.submit(&fork, "child marker").await;
    assert!(output.to_string().contains("parent marker"));
    assert!(output.to_string().contains("child answer"));
    let saved = session_json(&env.dir, &fork);
    let messages = saved["threads"][0]["messages"].as_array().unwrap();
    let own = messages
        .iter()
        .find(|message| message.to_string().contains("child marker"))
        .unwrap();
    assert_eq!(
        own["UserMessage"]["content"][0]["System"]["data"]["session_id"],
        fork
    );
    assert!(!output.to_string().contains("# Session"));
    assert_eq!(provider.connections(), 2);
    server.stop().await;
}

#[tokio::test]
async fn automatic_compaction_continues_without_inventing_human_input_and_replays_after_restart() {
    let env = ServerEnv::new("auto-compact");
    let saved = compact_test_session(&env);
    let provider = test_utils::StubHttpServer::sequence(vec![
        model_answer("working", 80_000),
        write_summary_response(&saved),
        model_answer("summary ready", 100),
        model_answer("continued task", 80_000),
    ])
    .await;
    configure_compact(&env, &provider, true);
    let server = Server::start(&env, &["--resume", &saved.id]).await;
    let output = server.submit(&saved.id, "finish task").await;
    assert!(output.to_string().contains("continued task"), "{output}");
    assert!(!output.to_string().contains("# Resumption"));
    let session = session_json(&env.dir, &saved.id);
    let threads = session["threads"].as_array().unwrap();
    assert_eq!(threads.len(), 2);
    assert_eq!(
        threads[1]["user_turn_timestamps"]
            .as_object()
            .unwrap()
            .len(),
        1
    );
    assert!(threads[1].to_string().contains("# Resumption"));
    assert_eq!(provider.connections(), 4);
    server.stop().await;
    let server = Server::start(&env, &[]).await;
    let replay = server.snapshot(&saved.id).await;
    assert!(replay.to_string().contains("continued task"));
    assert!(!replay.to_string().contains("# Resumption"));
    assert_eq!(provider.connections(), 4);
    server.stop().await;
}

#[tokio::test]
async fn manual_compaction_respects_its_budget_and_never_resumes_automatically() {
    for limit in [None, Some(2)] {
        let env = ServerEnv::new("manual-compact");
        let saved = compact_test_session(&env);
        let mut responses = vec![model_answer("done", 100)];
        responses.extend((0..13).map(|_| {
            model_tool(
                "session_history",
                json!({
                    "action":"stats", "session_id":saved.id, "thread_id":saved.active_thread().id,
                }),
                100,
            )
        }));
        responses.push(write_summary_response(&saved));
        responses.push(model_answer("summary ready", 100));
        let provider = test_utils::StubHttpServer::sequence(responses).await;
        configure_compact(&env, &provider, true);
        if let Some(limit) = limit {
            let config = std::fs::read_to_string(&env.config).unwrap();
            std::fs::write(
                &env.config,
                format!("compaction_max_requests = {limit}\n{config}"),
            )
            .unwrap();
        }
        let server = Server::start(&env, &["--resume", &saved.id]).await;
        server.submit(&saved.id, "task").await;
        server.action(&saved.id, json!({"kind":"compact"})).await;
        server.idle(&saved.id).await;
        let session = session_json(&env.dir, &saved.id);
        assert_eq!(
            session["threads"].as_array().unwrap().len(),
            if limit.is_some() { 1 } else { 2 }
        );
        assert!(!session.to_string().contains("# Resumption"));
        assert_eq!(provider.connections(), if limit.is_some() { 3 } else { 16 });
        server.stop().await;
    }
}

#[tokio::test]
async fn failed_compaction_or_usage_below_threshold_does_not_continue() {
    for lower_threshold in [false, true] {
        let env = ServerEnv::new("auto-stop");
        let provider = test_utils::StubHttpServer::sequence(vec![
            model_answer("done", 80_000),
            model_answer("no summary written", 100),
        ])
        .await;
        configure_compact(&env, &provider, lower_threshold);
        let server = Server::start(&env, &[]).await;
        let id = server.create(None, false).await;
        server.submit(&id, "task").await;
        let session = session_json(&env.dir, &id);
        assert_eq!(session["threads"].as_array().unwrap().len(), 1);
        assert!(!session.to_string().contains("# Resumption"));
        assert_eq!(provider.connections(), if lower_threshold { 2 } else { 1 });
        server.stop().await;
    }
}

#[tokio::test]
async fn cancellation_during_tools_or_compaction_stops_continuation_and_keeps_server_usable() {
    for compacting in [false, true] {
        let env = ServerEnv::new("cancel");
        let marker = env.dir.join("tool-started");
        let provider = if compacting {
            test_utils::StubHttpServer::sequence_then_pending(vec![model_answer("working", 80_000)])
                .await
        } else {
            test_utils::StubHttpServer::sequence(vec![model_tool(
                "bash",
                json!({"command":format!("touch '{}' && sleep 30", marker.display())}),
                80_000,
            )])
            .await
        };
        configure_compact(&env, &provider, true);
        let server = Server::start(&env, &[]).await;
        let id = server.create(None, false).await;
        server
            .action(&id, json!({"kind":"submit", "text":"task"}))
            .await;
        tokio::time::timeout(Duration::from_secs(10), async {
            while if compacting {
                provider.connections() < 2
            } else {
                !marker.exists()
            } {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            server
                .request(
                    "POST",
                    &format!("/api/sessions/{id}/cancel"),
                    json!({"session_id":id})
                )
                .await
                .0,
            204
        );
        server.idle(&id).await;
        let session = session_json(&env.dir, &id);
        assert_eq!(session["threads"].as_array().unwrap().len(), 1);
        assert!(!session.to_string().contains("# Resumption"));
        assert_eq!(provider.connections(), if compacting { 2 } else { 1 });
        assert_ne!(server.create(None, false).await, id);
        server.stop().await;
    }
}

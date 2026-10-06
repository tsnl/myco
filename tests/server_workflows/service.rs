//! Native clients share the server's runner, identity, and execution workspace.

use super::*;
use tokio::io::AsyncWriteExt;

fn cli(env: &ServerEnv, server: &Server, id: &str, args: &[&str]) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_myco"));
    command
        .args([
            "--server",
            &format!("{}/profiles/default", server.origin),
            "--resume",
            id,
        ])
        .args(args)
        .current_dir(&env.dir)
        // These values must never be consulted to start a competing local runner.
        .env("MYCO_HOME", env.dir.join("unused-client-home"))
        .env("MYCO_CONFIG", env.dir.join("missing-client-config"))
        .env("MYCO_PROFILE", "different")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

async fn run_cli(mut command: tokio::process::Command, input: &[u8]) -> std::process::Output {
    let mut child = command.spawn().unwrap();
    child.stdin.take().unwrap().write_all(input).await.unwrap();
    tokio::time::timeout(Duration::from_secs(20), child.wait_with_output())
        .await
        .unwrap()
        .unwrap()
}

fn token(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr)
        .lines()
        .find_map(|line| line.strip_prefix("run="))
        .expect("announced reconnect token")
        .into()
}

async fn identity(server: &Server) -> Value {
    let (status, body) = server
        .request("GET", "/profiles/default/api/service", Value::Null)
        .await;
    assert_eq!(status, 200, "{body}");
    body
}

#[tokio::test]
async fn native_cli_detaches_reconnects_and_resolves_images_only_in_the_client_cwd() {
    let env = ServerEnv::new("native-cli");
    let provider =
        test_utils::StubHttpServer::sequence(vec![model_answer("committed answer", 100)]).await;
    configure_compact(&env, &provider, false);
    std::fs::write(env.dir.join("client.png"), b"\x89PNG\r\n\x1a\n").unwrap();
    let server = Server::start(&env, &[]).await;
    let id = server.create(None, false).await;
    let detached = run_cli(
        cli(
            &env,
            &server,
            &id,
            &["--detach", "-p", "review @client.png"],
        ),
        b"piped @missing.png\n",
    )
    .await;
    assert!(detached.status.success(), "{detached:?}");
    assert!(detached.stdout.is_empty());
    let reconnect = token(&detached);
    let output = run_cli(cli(&env, &server, &id, &["--observe", &reconnect]), b"").await;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"committed answer\n");
    let replay = run_cli(cli(&env, &server, &id, &["--observe", &reconnect]), b"").await;
    assert_eq!(replay.stdout, output.stdout);
    assert_eq!(provider.connections(), 1);
    assert!(
        session_json(&env.dir, &id)
            .to_string()
            .contains("committed answer")
    );
    assert!(!env.dir.join("unused-client-home").exists());
    let request = provider.captured().await.body.to_string();
    assert!(request.contains("data:image/png;base64,"), "{request}");
    assert!(request.contains("piped @missing.png"), "{request}");
    server.stop().await;
}

#[tokio::test]
async fn failed_generation_checkpoint_keeps_unsaved_text_out_of_native_stdout_and_receipts() {
    let env = ServerEnv::new("native-checkpoint");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let config = std::fs::read_to_string(&env.config).unwrap().replace(
        "http://127.0.0.1:1/v1",
        &format!("http://{}/v1", listener.local_addr().unwrap()),
    );
    std::fs::write(&env.config, config).unwrap();
    let (started, requested) = tokio::sync::oneshot::channel();
    let (release, ready) = tokio::sync::oneshot::channel();
    let provider = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_provider_request(&mut stream).await;
        started.send(()).unwrap();
        ready.await.unwrap();
        stream
            .write_all(&model_answer("unsaved service answer", 100))
            .await
            .unwrap();
    });
    let server = Server::start(&env, &[]).await;
    let id = server.create(None, false).await;
    let client = tokio::spawn(run_cli(cli(&env, &server, &id, &["-p", "work"]), b""));
    tokio::time::timeout(Duration::from_secs(10), requested)
        .await
        .unwrap()
        .unwrap();

    // The generation intent is durable; make its result checkpoint fail.
    let store = env.dir.join("profiles/default/session");
    let backup = env.dir.join("saved-store");
    std::fs::rename(&store, &backup).unwrap();
    std::fs::write(&store, b"storage unavailable").unwrap();
    release.send(()).unwrap();
    let output = client.await.unwrap();
    provider.await.unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("could not persist"));
    let saved = std::fs::read_to_string(backup.join(&id[..2]).join(format!("{id}.json"))).unwrap();
    assert!(!saved.contains("unsaved service answer"));
    let reconnect = token(&output);
    let replay = run_cli(cli(&env, &server, &id, &["--observe", &reconnect]), b"").await;
    assert_eq!(replay.status.code(), Some(1), "{replay:?}");
    assert!(replay.stdout.is_empty(), "{replay:?}");

    std::fs::remove_file(&store).unwrap();
    std::fs::rename(backup, store).unwrap();
    server.stop().await;
}

async fn read_provider_request(stream: &mut tokio::net::TcpStream) {
    let mut request = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let count = stream.read(&mut buffer).await.unwrap();
        assert!(count > 0, "provider request ended early");
        request.extend_from_slice(&buffer[..count]);
        let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
            continue;
        };
        let headers = std::str::from_utf8(&request[..end]).unwrap();
        let length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            })
            .unwrap();
        if request.len() >= end + 4 + length {
            return;
        }
    }
}

#[tokio::test]
async fn lost_acceptance_response_retries_once_and_receipt_survives_compaction_but_not_restart() {
    let env = ServerEnv::new("native-receipts");
    let saved = compact_test_session(&env);
    let provider = test_utils::StubHttpServer::sequence(vec![
        model_answer("completed", 100),
        write_summary_response(&saved),
        model_answer("summary ready", 100),
    ])
    .await;
    configure_compact(&env, &provider, false);
    let server = Server::start(&env, &[]).await;
    let identity = identity(&server).await;
    let request_id = Uuid::new_v4();
    let body = json!({"instance":identity["instance"], "request_id":request_id, "text":"work"});
    let path = format!("/profiles/default/api/service/sessions/{}/turns", saved.id);
    let address = server.origin.strip_prefix("http://").unwrap();
    let mut connection = tokio::net::TcpStream::connect(address).await.unwrap();
    let data = body.to_string();
    connection.write_all(format!("POST {path} HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{data}", data.len()).as_bytes()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while provider.connections() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    // The request was accepted; the client never reads its HTTP response.
    drop(connection);
    assert_eq!(server.request("POST", &path, body.clone()).await.0, 202);
    server.idle(&saved.id).await;
    assert_eq!(provider.connections(), 1);
    let output_path = format!(
        "{path}/{request_id}/output?instance={}&offset=0",
        identity["instance"].as_str().unwrap()
    );
    let (status, output) = server.request("GET", &output_path, Value::Null).await;
    assert_eq!(status, 200, "{output}");
    assert_eq!(output["output"], "completed");
    assert_eq!(output["exit_code"], 0);
    server.action(&saved.id, json!({"kind":"compact"})).await;
    let compacted = server.idle(&saved.id).await;
    assert_ne!(compacted["thread_id"], saved.active_thread().id);
    assert_eq!(
        server.request("GET", &output_path, Value::Null).await.1,
        output
    );
    server.stop().await;
    let server = Server::start(&env, &[]).await;
    let (status, error) = server.request("POST", &path, body).await;
    assert_eq!(status, 409, "{error}");
    assert!(error.as_str().unwrap().contains("instance changed"));
    let reconnect = format!("{}:{request_id}", identity["instance"].as_str().unwrap());
    let output = run_cli(
        cli(&env, &server, &saved.id, &["--observe", &reconnect]),
        b"",
    )
    .await;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("instance changed"));
    assert_eq!(provider.connections(), 3);
    server.stop().await;
}

#[tokio::test]
async fn native_observer_ctrl_c_cancels_the_accepted_turn() {
    let env = ServerEnv::new("native-cancel");
    let provider = test_utils::StubHttpServer::sequence_then_pending(vec![]).await;
    configure_compact(&env, &provider, false);
    let server = Server::start(&env, &[]).await;
    let id = server.create(None, false).await;
    let output = run_cli(cli(&env, &server, &id, &["--detach", "-p", "work"]), b"").await;
    assert!(output.status.success(), "{output:?}");
    let reconnect = token(&output);
    let mut child = cli(&env, &server, &id, &["--observe", &reconnect])
        .spawn()
        .unwrap();
    child.stdin.take();
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut line = String::new();
    for _ in 0..3 {
        stderr.read_line(&mut line).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(150)).await;
    // SAFETY: child is owned and has not been waited on.
    assert_eq!(
        unsafe { libc::kill(child.id().unwrap() as libc::pid_t, libc::SIGINT) },
        0
    );
    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(output.status.code(), Some(130), "{output:?}");
    assert_eq!(server.idle(&id).await["busy"], false);
    assert_eq!(provider.connections(), 1);
    server.stop().await;
}

#[tokio::test]
async fn incompatible_or_unreachable_service_never_starts_a_local_runner() {
    let env = ServerEnv::new("native-incompatible");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/profiles/default", listener.local_addr().unwrap());
    let responder = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 1024];
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let count = stream.read(&mut buffer).await.unwrap();
            assert!(count > 0, "client disconnected before sending its request");
            request.extend_from_slice(&buffer[..count]);
        }
        let body = json!({"protocol":0, "version":"old", "build":"old", "instance":Uuid::new_v4(), "profile":"default", "workspace":"/tmp"}).to_string();
        stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
    });
    let session = Uuid::new_v4().simple().to_string();
    for message in ["Incompatible Myco service", "Cannot reach service"] {
        let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_myco"))
            .args(["--server", &url, "--resume", &session, "-p", "work"])
            .current_dir(&env.dir)
            .env("MYCO_HOME", env.dir.join("unused-client-home"))
            .env("MYCO_CONFIG", &env.config)
            .stdin(Stdio::null())
            .output()
            .await
            .unwrap();
        assert!(!output.status.success(), "{output:?}");
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(message),
            "{output:?}"
        );
        assert!(!env.dir.join("unused-client-home").exists());
    }
    responder.await.unwrap();
}

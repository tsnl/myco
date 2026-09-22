use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use uuid::Uuid;

struct TestHome(PathBuf);

impl Drop for TestHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Server {
    child: Child,
    _output: BufReader<tokio::process::ChildStdout>,
    address: String,
    cookie: String,
}

impl Server {
    async fn start(home: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_myco"))
            .args(["--web", "0", "--config"])
            .arg(home.join("config.toml"))
            .env("MYCO_HOME", home)
            .env("MYCO_PROFILE", "default")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(10), output.read_line(&mut line))
            .await
            .expect("browser startup must finish")
            .unwrap();
        let url = url::Url::parse(line.trim().strip_prefix("Browser UI: ").unwrap()).unwrap();
        let token = url
            .query_pairs()
            .find(|(key, _)| key == "token")
            .unwrap()
            .1
            .into_owned();
        Self {
            child,
            _output: output,
            address: format!("127.0.0.1:{}", url.port().unwrap()),
            cookie: format!("myco_{}={token}", url.port().unwrap()),
        }
    }

    async fn request(&self, path: &str, body: Option<Value>) -> (u16, String) {
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut socket = tokio::net::TcpStream::connect(&self.address).await.unwrap();
            let method = if body.is_some() { "POST" } else { "GET" };
            let body = body.map_or_else(String::new, |body| body.to_string());
            let request = format!(
                "{method} {path} HTTP/1.1\r\nHost: {}\r\nOrigin: http://{}\r\nCookie: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                self.address, self.address, self.cookie, body.len()
            );
            socket.write_all(request.as_bytes()).await.unwrap();
            let mut response = String::new();
            socket.read_to_string(&mut response).await.unwrap();
            let (headers, body) = response.split_once("\r\n\r\n").unwrap();
            let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
            (status, body.into())
        }).await.expect("browser request must finish")
    }

    async fn json(&self, path: &str, body: Option<Value>) -> Value {
        let (status, body) = self.request(path, body).await;
        assert_eq!(status, 200, "{path}: {body}");
        serde_json::from_str(&body).unwrap()
    }

    async fn stop(&mut self) {
        self.child.kill().await.unwrap();
    }
}

#[tokio::test]
async fn browser_session_urls_are_durable_and_retries_do_not_create_extra_sessions() {
    let home = TestHome(std::env::temp_dir().join(format!("myco-browser-{}", Uuid::new_v4())));
    std::fs::create_dir_all(&home.0).unwrap();
    let models = ["first", "second"]
        .map(|key| {
            format!(
                r#"
[models.{key}]
protocol = "openai-responses"
base_url = "http://127.0.0.1:1"
auth = {{ source = "none" }}
context_window = 100000
"#
            )
        })
        .join("\n");
    std::fs::write(
        home.0.join("config.toml"),
        format!("model = \"first\"\n{models}"),
    )
    .unwrap();
    let mut server = Server::start(&home.0).await;
    assert_eq!(server.json("/api/sessions", None).await, json!([]));
    let request_id = Uuid::new_v4();
    let create = json!({"request_id": request_id});
    let first = server.json("/api/sessions", Some(create.clone())).await;
    assert_eq!(first, server.json("/api/sessions", Some(create)).await);
    let first_id = first["id"].as_str().unwrap();
    assert!(
        home.0
            .join(format!(
                "profiles/default/session/{}/{first_id}.json",
                &first_id[..2]
            ))
            .exists()
    );
    let first_path = format!("/api/sessions/{first_id}");
    let prefix_path = format!("/api/sessions/{}", &first_id[..12]);
    let (full, prefix) = tokio::join!(
        server.json(&first_path, None),
        server.json(&prefix_path, None)
    );
    assert_eq!(full["session_id"], prefix["session_id"]);
    let second = server
        .json("/api/sessions", Some(json!({"request_id":Uuid::new_v4()})))
        .await;
    let second_path = format!("/api/sessions/{}", second["id"].as_str().unwrap());
    let (status, body) = server
        .request(
            &format!("{first_path}/action"),
            Some(json!({
                "request_id":Uuid::new_v4(), "session_id":first_id,
                "action":{"kind":"select_model", "key":"second"}
            })),
        )
        .await;
    assert_eq!(status, 202, "{body}");
    let selected = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = server.json(&first_path, None).await;
            if snapshot["change"]["snapshot"]["busy"] == false {
                break snapshot;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(selected["change"]["snapshot"]["model"], "second");
    assert_eq!(
        server.json(&second_path, None).await["change"]["snapshot"]["model"],
        "first"
    );
    assert_eq!(
        server
            .json("/api/sessions", None)
            .await
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let mut other = Server::start(&home.0).await;
    let (status, error) = other.request(&first_path, None).await;
    assert_eq!(status, 409);
    assert!(
        error.contains("already open in another myco process"),
        "{error}"
    );
    assert_eq!(
        other.request("/api/sessions/does-not-exist", None).await.0,
        404
    );
    assert!(other.child.try_wait().unwrap().is_none());
    other
        .json("/api/sessions", Some(json!({"request_id":Uuid::new_v4()})))
        .await;
    server.stop().await;
    assert_eq!(other.json(&first_path, None).await["session_id"], first_id);
    other.stop().await;
}

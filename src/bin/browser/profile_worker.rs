//! A profile owns one process and its private transport. Environment-based paths
//! and tool children therefore cannot change profile while requests are running.

use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{HeaderMap, header};
use axum::response::Response;
use futures::TryStreamExt;
use myco::CancelToken;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, broadcast};
use tokio_util::io::StreamReader;
use uuid::Uuid;

use super::super::Args;

//
// Worker lifecycle
//

#[derive(Deserialize, Serialize)]
pub(super) struct Ready {
    pub launch_path: String,
}

struct SocketDirectory(PathBuf);

impl SocketDirectory {
    fn new() -> Result<Self, String> {
        let id = Uuid::new_v4().simple().to_string();
        let path = std::env::temp_dir().join(format!("myco-{}", &id[..16]));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(|e| e.to_string())?;
        Ok(Self(path))
    }
}

impl Drop for SocketDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(super) struct Worker {
    child: Mutex<Child>,
    client: reqwest::Client,
    shutdown: CancelToken,
    pub launch_path: String,
    _directory: SocketDirectory,
}

impl Worker {
    pub async fn start(
        args: &Args,
        profile: &str,
        workspace: &Path,
        selected: &str,
        origin: &str,
        events: broadcast::Sender<Arc<ProfileEvent>>,
    ) -> Result<Arc<Self>, String> {
        tokio::fs::create_dir_all(workspace)
            .await
            .map_err(|e| format!("open profile workspace {}: {e}", workspace.display()))?;
        let directory = SocketDirectory::new()?;
        let socket = directory.0.join("worker");
        let mut command = Command::new(std::env::current_exe().map_err(|e| e.to_string())?);
        command
            .current_dir(workspace)
            .args(["--profile", profile, "--profile-worker"])
            .arg(&socket)
            .env("MYCO_PROFILE", profile)
            .env("MYCO_SERVER_URL", format!("{origin}/profiles/{profile}"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        configure(&mut command, args, profile == selected)?;
        let mut child = command
            .spawn()
            .map_err(|e| format!("start profile {profile}: {e}"))?;
        let mut output = BufReader::new(child.stdout.take().ok_or("worker stdout unavailable")?);
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(15), output.read_line(&mut line))
            .await
            .map_err(|_| format!("profile {profile} took too long to start"))?
            .map_err(|e| format!("profile {profile} startup: {e}"))?;
        let ready: Ready = serde_json::from_str(&line).map_err(|_| format!("Profile {profile} could not start. Check its config and the server's diagnostic output."))?;
        let client = reqwest::Client::builder()
            .unix_socket(socket)
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(2))
            .build()
            .map_err(|e| e.to_string())?;
        let shutdown = CancelToken::new();
        tokio::spawn(relay(
            client.clone(),
            profile.into(),
            events,
            shutdown.clone(),
        ));
        Ok(Arc::new(Self {
            child: Mutex::new(child),
            client,
            shutdown,
            launch_path: ready.launch_path,
            _directory: directory,
        }))
    }

    pub async fn exited(&self) -> Result<bool, String> {
        self.child
            .lock()
            .await
            .try_wait()
            .map(|status| status.is_some())
            .map_err(|e| e.to_string())
    }

    pub async fn stop(&self) {
        self.shutdown.cancel();
        let mut child = self.child.lock().await;
        drop(child.stdin.take());
        if tokio::time::timeout(Duration::from_secs(7), child.wait())
            .await
            .is_err()
        {
            eprintln!("profile worker did not stop promptly; terminating it");
            let _ = child.kill().await;
        }
    }

    pub async fn forward(&self, path: &str, request: Request) -> Result<Response, String> {
        let (mut parts, body) = request.into_parts();
        strip_hop_headers(&mut parts.headers);
        let response = self
            .client
            .request(parts.method, format!("http://localhost{path}"))
            .headers(parts.headers)
            .body(reqwest::Body::wrap_stream(body.into_data_stream()))
            .send()
            .await
            .map_err(|e| format!("Profile worker unavailable: {e}"))?;
        let status = response.status();
        let mut headers = response.headers().clone();
        strip_hop_headers(&mut headers);
        let mut outgoing = Response::new(Body::from_stream(response.bytes_stream()));
        *outgoing.status_mut() = status;
        *outgoing.headers_mut() = headers;
        Ok(outgoing)
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

fn configure(command: &mut Command, args: &Args, selected: bool) -> Result<(), String> {
    if !selected {
        command.env_remove("MYCO_CONFIG");
        return Ok(());
    }
    if let Some(config) = &args.config {
        command
            .arg("--config")
            .arg(std::path::absolute(config).map_err(|e| e.to_string())?);
    }
    if let Some(config) = std::env::var_os("MYCO_CONFIG") {
        // Launch overrides keep their meaning after the worker changes cwd.
        command.env(
            "MYCO_CONFIG",
            std::path::absolute(config).map_err(|e| e.to_string())?,
        );
    }
    if let Some(model) = &args.model {
        command.args(["--model", model]);
    }
    if let Some(resume) = &args.resume {
        command.args(["--resume", resume]);
    }
    command.args(["--effort", &args.effort.to_string()]);
    if args.debug_dump_api_requests {
        command.arg("--debug-dump-api-requests");
    }
    Ok(())
}

fn strip_hop_headers(headers: &mut HeaderMap) {
    let named: Vec<_> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(',').map(|name| name.trim().to_owned()))
        .collect();
    for name in named {
        headers.remove(name);
    }
    for name in [
        "connection",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ] {
        headers.remove(name);
    }
}

//
// One browser stream across every profile
//

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum ProfileEvent {
    Update {
        profile: String,
        update: Value,
    },
    // Existing tabs understand resync and refetch their snapshots. Updated tabs
    // use the revision and session ID to refresh only the affected view.
    #[serde(rename = "resync")]
    Refresh {
        profile: String,
        update: Value,
    },
    Connection {
        profile: String,
        connected: bool,
    },
    Resync {
        #[serde(skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
    },
}

fn profile_update(profile: &str, update: Value) -> ProfileEvent {
    let profile = profile.into();
    if update["kind"] == "resync" {
        ProfileEvent::Resync {
            profile: Some(profile),
        }
    } else if update["change"]["kind"] == "refresh" {
        ProfileEvent::Refresh { profile, update }
    } else {
        ProfileEvent::Update { profile, update }
    }
}

async fn relay(
    client: reqwest::Client,
    profile: String,
    events: broadcast::Sender<Arc<ProfileEvent>>,
    shutdown: CancelToken,
) {
    loop {
        let work = relay_connection(&client, &profile, &events);
        tokio::select! {
            _ = shutdown.cancelled() => return,
            result = work => if let Err(error) = result { eprintln!("profile {profile} event stream: {error}"); },
        }
        let _ = events.send(Arc::new(ProfileEvent::Connection {
            profile: profile.clone(),
            connected: false,
        }));
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(Duration::from_secs(2)) => {},
        }
    }
}

async fn relay_connection(
    client: &reqwest::Client,
    profile: &str,
    events: &broadcast::Sender<Arc<ProfileEvent>>,
) -> Result<(), String> {
    let response = client
        .get(format!(
            "http://localhost/profiles/{profile}/api/live-events"
        ))
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| e.to_string())?;
    let _ = events.send(Arc::new(ProfileEvent::Connection {
        profile: profile.into(),
        connected: true,
    }));
    let stream = response.bytes_stream().map_err(std::io::Error::other);
    let mut lines = BufReader::new(StreamReader::new(stream)).lines();
    while let Some(line) = lines.next_line().await.map_err(|e| e.to_string())? {
        // The embedded server emits one compact JSON data line per update.
        if let Some(data) = line.strip_prefix("data:") {
            let update: Value =
                serde_json::from_str(data).map_err(|e| format!("invalid worker event: {e}"))?;
            let _ = events.send(Arc::new(profile_update(profile, update)));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_refresh_remains_a_resync_for_tabs_opened_before_a_server_upgrade() {
        let update = serde_json::json!({
            "session_id":"session", "revision":42, "change":{"kind":"refresh"}
        });
        let event = serde_json::to_value(profile_update("work", update.clone())).unwrap();
        assert_eq!(event["kind"], "resync");
        assert_eq!(event["profile"], "work");
        assert_eq!(event["update"], update);
        let lag =
            serde_json::to_value(profile_update("work", serde_json::json!({"kind":"resync"})))
                .unwrap();
        assert_eq!(lag, serde_json::json!({"kind":"resync", "profile":"work"}));
    }

    #[test]
    fn proxy_removes_connection_headers_but_preserves_origin_and_file_contracts() {
        let mut headers = HeaderMap::new();
        for (name, value) in [
            ("connection", "keep-alive, x-private"),
            ("x-private", "connection-only"),
            ("transfer-encoding", "chunked"),
            ("host", "localhost:9999"),
            ("origin", "http://localhost:9999"),
            ("range", "bytes=2-5"),
            ("content-security-policy", "sandbox"),
        ] {
            headers.insert(name, value.parse().unwrap());
        }
        strip_hop_headers(&mut headers);
        for name in ["connection", "x-private", "transfer-encoding"] {
            assert!(!headers.contains_key(name));
        }
        for name in ["host", "origin", "range", "content-security-policy"] {
            assert!(headers.contains_key(name));
        }
    }
}

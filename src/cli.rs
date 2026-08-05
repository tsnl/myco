//! `myco -p "prompt"`: one agent turn over the HTTP API, answer to stdout.
//!
//! This is a *client*. There is exactly one door into the runtime — the REST
//! API served by `myco --mode serve` — and this walks through it like the web
//! GUI and `clients/myco.py` do, authenticating with the operator token the
//! server writes to `$MYCO_HOME/v2/operator.token`. If no server is running
//! on the target port, one is spawned (detached) and left running.

use std::time::Duration;

use myco_api::MycoApi;

use crate::client::HttpClient;

pub struct PrintOptions {
    pub prompt: String,
    /// Catalog model key for a fresh session (`None` = server default).
    pub model: Option<String>,
    /// Continue an existing session instead of creating one.
    pub session: Option<String>,
    pub port: u16,
}

/// How long to wait for a just-spawned server to answer (config resolve +
/// model boot can take a moment on a cold machine).
const SPAWN_WAIT: Duration = Duration::from_secs(30);

pub async fn run_print(opts: PrintOptions) -> i32 {
    let base =
        std::env::var("MYCO_API").unwrap_or_else(|_| format!("http://127.0.0.1:{}/api", opts.port));

    let client = match connect(&base, opts.port).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("myco: {e}");
            return 1;
        }
    };

    let session_id = match &opts.session {
        Some(id) => id.clone(),
        None => match client
            .create_session(myco_api::CreateSession {
                model: opts.model.clone(),
                parent_session: None,
                fork: false,
            })
            .await
        {
            Ok(s) => s.id,
            Err(e) => {
                eprintln!("myco: create session: {e}");
                return 1;
            }
        },
    };

    if let Err(e) = client
        .post_message(
            &session_id,
            myco_api::PostMessage {
                text: opts.prompt.clone(),
            },
        )
        .await
    {
        eprintln!("myco: send: {e}");
        return 1;
    }

    // Poll until the turn settles. No overall deadline: a long build is the
    // agent working, not a hang — Ctrl-C is the escape hatch.
    let final_poll = loop {
        tokio::time::sleep(Duration::from_millis(500)).await;
        match client.poll(&session_id, 0).await {
            Ok(p) if !p.busy => break p,
            Ok(_) => continue,
            Err(e) => {
                eprintln!("myco: poll: {e}");
                return 1;
            }
        }
    };

    eprintln!("session={session_id}");
    if let Some(err) = &final_poll.last_error {
        eprintln!("myco: {err}");
        return 1;
    }
    match crate::server::last_answer(&final_poll.entries) {
        Some(text) => println!("{text}"),
        None => eprintln!("(no answer text)"),
    }
    0
}

/// An authenticated client for `base`, spawning a local server when nothing
/// answers there and `base` is this machine's default endpoint.
async fn connect(base: &str, port: u16) -> Result<HttpClient, String> {
    if let Ok(client) = authed(base).await {
        return Ok(client);
    }

    let local_default = base == format!("http://127.0.0.1:{port}/api");
    if !local_default {
        return Err(format!(
            "no myco server answering at {base} (is it running, and does \
             operator.token match it?)"
        ));
    }

    spawn_server(port)?;
    let deadline = std::time::Instant::now() + SPAWN_WAIT;
    loop {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if let Ok(client) = authed(base).await {
            return Ok(client);
        }
        if std::time::Instant::now() > deadline {
            return Err(format!(
                "spawned `myco --mode serve --port {port}` but it never answered; \
                 check `{}`",
                server_log_path().display()
            ));
        }
    }
}

/// A client whose token the server actually accepts (`whoami` proves both
/// reachability and that operator.token is this server's, not a stale one).
async fn authed(base: &str) -> Result<HttpClient, String> {
    let token = match std::env::var("MYCO_API_TOKEN") {
        Ok(t) => t,
        Err(_) => {
            let path = crate::web::operator_token_path()?;
            std::fs::read_to_string(&path)
                .map_err(|e| format!("read {}: {e}", path.display()))?
                .trim()
                .to_string()
        }
    };
    let client = HttpClient::new(base, token);
    client.whoami().await.map_err(|e| e.to_string())?;
    Ok(client)
}

fn server_log_path() -> std::path::PathBuf {
    crate::core::data_root()
        .map(|d| d.join("serve.log"))
        .unwrap_or_else(|_| std::path::PathBuf::from("myco-serve.log"))
}

/// Spawn `myco --mode serve` detached (own process group, output to a log
/// file) and leave it running — the next `-p`, the GUI, and scripts all get
/// to reuse it.
fn spawn_server(port: u16) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(server_log_path())
        .map_err(|e| format!("open serve log: {e}"))?;
    eprintln!("myco: no server on port {port}; starting one");
    std::process::Command::new(exe)
        .args(["--mode", "serve", "--port", &port.to_string()])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(
            log.try_clone().map_err(|e| e.to_string())?,
        ))
        .stderr(std::process::Stdio::from(log))
        .process_group(0)
        .spawn()
        .map_err(|e| format!("spawn serve: {e}"))?;
    Ok(())
}

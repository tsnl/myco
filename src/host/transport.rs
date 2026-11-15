//! NDJSON pipe transport to a `myco --mode host` subprocess: spawn and
//! version handshake, then one writer task and one demux reader task per
//! connection.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::host::host_controller::{Activity, HostConfig};
use crate::host::protocol::{HOST_PROTOCOL_VERSION, Request, Response, read_line};

/// Live child + demux state. Drop aborts I/O tasks and kills the child.
pub(super) struct Conn {
    pub(super) child: Child,
    pub(super) write_tx: mpsc::Sender<Vec<u8>>,
    pub(super) pending: Arc<Mutex<HashMap<String, oneshot::Sender<Response>>>>,
    pub(super) dead: Arc<AtomicBool>,
    pub(super) activity: Arc<StdMutex<Activity>>,
    reader_abort: tokio::task::AbortHandle,
    writer_abort: tokio::task::AbortHandle,
}

impl Drop for Conn {
    fn drop(&mut self) {
        self.reader_abort.abort();
        self.writer_abort.abort();
        let _ = self.child.start_kill();
        if let Ok(mut pending) = self.pending.try_lock() {
            for (_, tx) in pending.drain() {
                let _ = tx.send(Response::Error {
                    id: None,
                    message: "host connection closed".into(),
                });
            }
        }
    }
}

pub(super) async fn connect_with_timeout(
    config: &HostConfig,
    connect_timeout_secs: u64,
) -> Result<Conn, String> {
    let fut = connect(config);
    match connect_timeout_secs {
        0 => fut.await,
        secs => match tokio::time::timeout(Duration::from_secs(secs), fut).await {
            Ok(r) => r,
            Err(_) => Err(format!("connect timed out after {secs}s")),
        },
    }
}

async fn connect(config: &HostConfig) -> Result<Conn, String> {
    if config.command.is_empty() {
        return Err(format!("host {:?}: empty command", config.name));
    }
    let program = &config.command[0];
    let args = &config.command[1..];

    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("spawn {:?}: {e}", config.command))?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| format!("host {:?}: missing stdin", config.name))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("host {:?}: missing stdout", config.name))?;
    let mut stdout = BufReader::new(stdout);

    // Hello before demux tasks.
    let hello = Request::Hello.encode()?;
    write_all(&mut stdin, &hello).await?;
    let line = read_line(&mut stdout).await?.ok_or("peer closed")?;
    let reply = Response::decode(&line)?;

    let (version, protocol) = match reply {
        Response::HelloOk { version, protocol } => (version, protocol),
        Response::Error { message, .. } => {
            let _ = child.start_kill();
            return Err(format!("hello error: {message}"));
        }
        other => {
            let _ = child.start_kill();
            return Err(format!("unexpected hello reply: {other:?}"));
        }
    };

    // Same-version lockstep: the assumed tool catalog and the NDJSON protocol
    // are only sound when both ends run the same build, so skew is a connect
    // error — not a latent tool failure hours later.
    let local_version = env!("CARGO_PKG_VERSION");
    if version != local_version {
        let _ = child.start_kill();
        return Err(format!(
            "remote myco {version} does not match local {local_version}; \
             rebuild myco on host {:?} (see harness-ops.md in the manual)",
            config.name
        ));
    }
    if protocol != HOST_PROTOCOL_VERSION {
        let _ = child.start_kill();
        return Err(format!(
            "remote host protocol {protocol} does not match local {HOST_PROTOCOL_VERSION}; rebuild myco on host {:?}",
            config.name
        ));
    }

    let pending: Arc<Mutex<HashMap<String, oneshot::Sender<Response>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let pending_reader = Arc::clone(&pending);
    let activity = Arc::new(StdMutex::new(Activity::default()));
    let reader_activity = Arc::clone(&activity);
    let pending_writer = Arc::clone(&pending);
    let dead = Arc::new(AtomicBool::new(false));
    let writer_dead = Arc::clone(&dead);
    let reader_dead = Arc::clone(&dead);
    let (write_tx, write_rx) = mpsc::channel::<Vec<u8>>(64);

    let writer = tokio::spawn(async move {
        run_writer(stdin, write_rx, pending_writer, writer_dead).await;
    });
    let reader = tokio::spawn(async move {
        run_reader(stdout, pending_reader, reader_dead, reader_activity).await;
    });

    let conn = Conn {
        child,
        write_tx,
        pending,
        dead,
        activity,
        reader_abort: reader.abort_handle(),
        writer_abort: writer.abort_handle(),
    };
    Ok(conn)
}

async fn write_all(w: &mut ChildStdin, bytes: &[u8]) -> Result<(), String> {
    w.write_all(bytes)
        .await
        .map_err(|e| format!("write: {e}"))?;
    w.flush().await.map_err(|e| format!("flush: {e}"))?;
    Ok(())
}

async fn run_writer(
    mut stdin: ChildStdin,
    mut rx: mpsc::Receiver<Vec<u8>>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Response>>>>,
    dead: Arc<AtomicBool>,
) {
    let exit_message = loop {
        let Some(bytes) = rx.recv().await else {
            break "host write channel closed".to_string();
        };
        if write_all(&mut stdin, &bytes).await.is_err() {
            break "host write failed".to_string();
        }
    };
    // Same contract as run_reader: poison first, then drain. A waiter that
    // registered after the reader already drained (host died mid-hello) but
    // before this write failed must still be failed, not left hanging.
    dead.store(true, Ordering::SeqCst);
    let mut pending = pending.lock().await;
    for (_, tx) in pending.drain() {
        let _ = tx.send(Response::Error {
            id: None,
            message: exit_message.clone(),
        });
    }
}

/// Demux loop. Exits on EOF or on a connection-fatal `Error{id:None}` from
/// the worker (undecodable request — agent/worker version skew); either way
/// the connection is poisoned: `dead` is set *before* draining `pending`
/// (see `submit` for the pairing), then every waiter gets an error instead
/// of hanging on a reply that can no longer come.
async fn run_reader(
    mut stdout: BufReader<ChildStdout>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<Response>>>>,
    dead: Arc<AtomicBool>,
    activity: Arc<StdMutex<Activity>>,
) {
    let exit_message = loop {
        let line = match read_line(&mut stdout).await {
            Ok(Some(l)) => l,
            Ok(None) | Err(_) => break "host closed stdout".to_string(),
        };
        let msg = match Response::decode(&line) {
            Ok(m) => m,
            Err(_) => continue,
        };

        if let Response::Error { id: None, message } = &msg {
            break format!("host protocol error: {message}");
        }
        let Some(id) = msg.id().map(str::to_owned) else {
            continue;
        };
        activity.lock().unwrap().finish(&id);
        if let Some(tx) = pending.lock().await.remove(&id) {
            let _ = tx.send(msg);
        }
    };

    dead.store(true, Ordering::SeqCst);
    let mut pending = pending.lock().await;
    for (_, tx) in pending.drain() {
        let _ = tx.send(Response::Error {
            id: None,
            message: exit_message.clone(),
        });
    }
}

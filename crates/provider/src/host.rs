//! The `host` kind: a machine (or any provider process) as an instance.
//! The instance owns the connection's whole lifecycle — spawn the
//! command, attach, relay, and dial again when the stream dies — as its
//! side-feed, with status as readable state. Creation-over-there is a
//! verb (`new`) on the host, not a change to `sys.new`: the palette and
//! every client learn "create on this machine" from the spec, and L2
//! never hears about providers at all.
//!
//! Doctrine notes. The dial loop is the side-feed pattern verbatim:
//! status lives in [`Shared`], the stream's death is a mutation plus an
//! event, and watchers re-read. The seat is not involved — `new` and
//! `reconnect` are plain writes, because a host is shared infrastructure
//! like the pool itself, not a surface one person drives. And a removed
//! host takes its adopted rows with it: `Drop` sweeps by origin, the
//! same cleanup a dead stream runs.

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use myco_instance::{
    CreateCtx, Instance, Kind, KindSpec, Pool, Principal, Shared, VerbError, VerbSpec,
};
use myco_runtime::Signals;
use serde_json::{Value, json};

use crate::attach::{Link, attach};

static HOST_SPEC: KindSpec = KindSpec {
    kind: "host",
    version: 1,
    doc: "a machine serving its kinds over a provider stream (ssh + myco-hostd); \
          holds the connection, shows its status, creates instances over there",
    verbs: &[
        VerbSpec::read("about", "connection status, the provider's name and offers"),
        VerbSpec::read("text", "the status, plainly"),
        VerbSpec::write("new", "create an instance over there: {kind, title?, project?, args?}"),
        VerbSpec::write("reconnect", "drop the stream and dial again now"),
    ],
    primary_render: "about",
    recommended_context: "text",
};

pub struct HostKind {
    pool: Pool,
}

impl HostKind {
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }
}

impl Kind for HostKind {
    fn spec(&self) -> &'static KindSpec {
        &HOST_SPEC
    }

    fn create(
        &self,
        ctx: &CreateCtx,
        args: Value,
        signals: Signals,
    ) -> Result<Box<dyn Instance>, VerbError> {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .ok_or_else(|| VerbError::BadArgs {
                why: "a host needs {command} — the line that reaches its provider \
                      (e.g. \"ssh box myco-hostd\")"
                    .into(),
            })?
            .to_string();

        let shared = Shared::new(
            HostState {
                status: "dialing",
                detail: String::new(),
                name: None,
                offers: Vec::new(),
            },
            signals,
        );
        let link: Arc<Mutex<Option<Link>>> = Arc::new(Mutex::new(None));
        let redial = Arc::new(tokio::sync::Notify::new());

        let task = tokio::spawn(dial_forever(
            self.pool.clone(),
            ctx.id.clone(),
            command.clone(),
            shared.clone(),
            Arc::clone(&link),
            Arc::clone(&redial),
        ));

        Ok(Box::new(Host {
            id: ctx.id.clone(),
            pool: self.pool.clone(),
            command,
            shared,
            link,
            redial,
            task,
        }))
    }
}

struct HostState {
    status: &'static str,
    /// Why it is down, when it is; empty otherwise.
    detail: String,
    /// The provider's announced name, kept across disconnects for display.
    name: Option<String>,
    /// (kind, vocabulary version) pairs from the last hello. The full
    /// specs live one forward away: `sys.spec` on any adopted instance.
    offers: Vec<(String, u32)>,
}

struct Host {
    id: String,
    pool: Pool,
    command: String,
    shared: Shared<HostState>,
    link: Arc<Mutex<Option<Link>>>,
    redial: Arc<tokio::sync::Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Host {
    fn drop(&mut self) {
        // Aborting the dial task drops the attachment (rows swept, child
        // killed on drop); the direct sweep covers the window where the
        // abort lands between frames. Both are idempotent.
        self.task.abort();
        self.pool.drop_remotes_from(&self.id);
    }
}

#[async_trait::async_trait]
impl Instance for Host {
    async fn verb(
        &mut self,
        caller: &Principal,
        verb: &str,
        args: Value,
        _signals: &Signals,
    ) -> Result<Value, VerbError> {
        match verb {
            "about" => Ok(self.shared.read(|s| {
                json!({
                    "command": self.command,
                    "status": s.status,
                    "detail": s.detail,
                    "name": s.name,
                    "kinds": s.offers.iter()
                        .map(|(kind, version)| json!({"kind": kind, "version": version}))
                        .collect::<Vec<_>>(),
                })
            })),
            "text" => Ok(Value::String(self.shared.read(|s| {
                let who = s.name.as_deref().unwrap_or("(unnamed)");
                match s.status {
                    "up" => format!("host {who}: up — {}", self.command),
                    "dialing" => format!("host {who}: dialing — {}", self.command),
                    _ => format!("host {who}: down ({}) — {}", s.detail, self.command),
                }
            }))),
            "new" => {
                let link = self
                    .link
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
                    .ok_or_else(|| VerbError::Denied {
                        why: "the host is not connected".into(),
                    })?;
                let kind = args
                    .get("kind")
                    .and_then(Value::as_str)
                    .ok_or_else(|| VerbError::BadArgs {
                        why: "new needs {kind}".into(),
                    })?;
                let title = args.get("title").and_then(Value::as_str).unwrap_or("");
                let project = args.get("project").and_then(Value::as_str).unwrap_or("");
                let create_args = args.get("args").cloned().unwrap_or(json!({}));
                // The caller crosses the wire as creator: the far pool's
                // seat and ownership bind to the person, not to the host.
                let info = link
                    .create(caller, kind, project, title, create_args)
                    .await?;
                Ok(serde_json::to_value(info).expect("rows serialize"))
            }
            "reconnect" => {
                self.redial.notify_one();
                Ok(Value::Null)
            }
            other => Err(VerbError::UnknownVerb { verb: other.into() }),
        }
    }
}

/// The dial loop: spawn, attach, relay, mourn, wait, again. Backoff
/// doubles to a minute and resets whenever a connection actually came
/// up — or when someone asks for a reconnect, which also skips the wait.
async fn dial_forever(
    pool: Pool,
    id: String,
    command: String,
    shared: Shared<HostState>,
    link_slot: Arc<Mutex<Option<Link>>>,
    redial: Arc<tokio::sync::Notify>,
) {
    let mut backoff = Duration::from_secs(1);
    loop {
        shared.with(|s| s.status = "dialing");
        let came_up = dial_once(&pool, &id, &command, &shared, &link_slot, &redial).await;
        if came_up {
            backoff = Duration::from_secs(1);
        }
        tokio::select! {
            _ = redial.notified() => {
                backoff = Duration::from_secs(1);
            }
            _ = tokio::time::sleep(backoff) => {
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
        }
    }
}

/// One connection, cradle to grave. Returns whether it ever came up.
async fn dial_once(
    pool: &Pool,
    id: &str,
    command: &str,
    shared: &Shared<HostState>,
    link_slot: &Arc<Mutex<Option<Link>>>,
    redial: &Arc<tokio::sync::Notify>,
) -> bool {
    let spawned = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(e) => {
            shared.with(|s| {
                s.status = "down";
                s.detail = format!("spawn: {e}");
            });
            return false;
        }
    };
    let stdout = child.stdout.take().expect("stdout piped");
    let stdin = child.stdin.take().expect("stdin piped");

    let attached = match attach(pool.clone(), id, stdout, stdin).await {
        Ok(attached) => attached,
        Err(e) => {
            shared.with(|s| {
                s.status = "down";
                s.detail = e.to_string();
            });
            return false;
        }
    };

    shared.with(|s| {
        s.status = "up";
        s.detail = String::new();
        s.name = Some(attached.name.clone());
        s.offers = attached
            .kinds
            .iter()
            .map(|offer| (offer.kind.clone(), offer.version))
            .collect();
    });
    shared
        .signals()
        .emit("connected", json!({ "name": attached.name }));
    *link_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(attached.link.clone());

    // Relay until the stream dies — or someone asks for a fresh dial,
    // which cancels the run and lets the attachment's drop do its sweep.
    let ended = tokio::select! {
        outcome = attached.run() => match outcome {
            Ok(()) => "the provider hung up".to_string(),
            Err(e) => e.to_string(),
        },
        _ = redial.notified() => "reconnect requested".to_string(),
    };

    *link_slot.lock().unwrap_or_else(|e| e.into_inner()) = None;
    shared.with(|s| {
        s.status = "down";
        s.detail = ended.clone();
    });
    shared.signals().emit("disconnected", json!({ "why": ended }));
    true
}

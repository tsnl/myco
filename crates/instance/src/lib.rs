//! L1: the instance framework — kinds, verbs, principals, drivers, and the
//! [`Pool`] that fronts them as one bus.
//!
//! Everything is a verb. Reads are verbs (`read_only`, always concurrent,
//! never driver-gated); writes are verbs; driver transfer and introspection
//! are the `sys.*` verbs the framework answers itself so kinds cannot get
//! them wrong. Authorization is one function of (principal, verb spec,
//! driver state): where a kind declares `requires_driver`, the check is
//! `driver == principal`, enforced by refusal — never by blocking.
//!
//! The driver is authority, not a mutex: durable, visible state on the
//! instance, changed by `sys.take`/`sys.release` under one policy — humans
//! may take from agents or from nobody; an agent may never take from a
//! human; release returns the instance to its default driver (its creator),
//! or to nobody when the default driver itself releases.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, RwLock};

use myco_runtime::{Cell, CellGone, Ctx, Event, Watermark};
use serde_json::{Value, json};

/// Who is asking. An agent is named by the chat instance it drives — both
/// adapters (HTTP for humans, the model loop for agents) resolve to this
/// before anything touches the bus.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum Principal {
    Human(String),
    Agent(String),
    System(String),
}

impl std::fmt::Display for Principal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Principal::Human(id) => write!(f, "{id}"),
            Principal::Agent(id) => write!(f, "agent:{id}"),
            Principal::System(id) => write!(f, "system:{id}"),
        }
    }
}

/// One verb in a kind's vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct VerbSpec {
    pub name: &'static str,
    pub doc: &'static str,
    /// Pure: no state change, never driver-gated, any number of concurrent
    /// callers.
    pub read_only: bool,
    /// Only the current driver may call it.
    pub requires_driver: bool,
    /// Takes a cursor and returns the next one, so callers read deltas.
    pub cursored: bool,
}

impl VerbSpec {
    pub const fn read(name: &'static str, doc: &'static str) -> Self {
        Self {
            name,
            doc,
            read_only: true,
            requires_driver: false,
            cursored: false,
        }
    }

    pub const fn cursored_read(name: &'static str, doc: &'static str) -> Self {
        Self {
            name,
            doc,
            read_only: true,
            requires_driver: false,
            cursored: true,
        }
    }

    pub const fn driven(name: &'static str, doc: &'static str) -> Self {
        Self {
            name,
            doc,
            read_only: false,
            requires_driver: true,
            cursored: false,
        }
    }

    pub const fn write(name: &'static str, doc: &'static str) -> Self {
        Self {
            name,
            doc,
            read_only: false,
            requires_driver: false,
            cursored: false,
        }
    }
}

/// A kind's contract: its verb vocabulary plus the two hints consumers use
/// to pick a default read — `primary_render` for pane renderers,
/// `recommended_context` for an agent's context assembly. Every kind must
/// include at least one plain-text read (the acme escape hatch; see
/// DESIGN.md).
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct KindSpec {
    pub kind: &'static str,
    pub doc: &'static str,
    pub verbs: &'static [VerbSpec],
    pub primary_render: &'static str,
    pub recommended_context: &'static str,
}

impl KindSpec {
    pub fn verb(&self, name: &str) -> Option<&'static VerbSpec> {
        self.verbs.iter().find(|v| v.name == name)
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum VerbError {
    UnknownKind {
        kind: String,
    },
    UnknownInstance {
        id: String,
    },
    UnknownVerb {
        verb: String,
    },
    /// Refused, not blocked: the caller is not the driver.
    NotDriver {
        held_by: Option<Principal>,
    },
    Denied {
        why: String,
    },
    BadArgs {
        why: String,
    },
    Failed {
        why: String,
    },
    /// The instance's task is dead (panicked kind, or removed mid-call).
    Gone,
}

impl std::fmt::Display for VerbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VerbError::UnknownKind { kind } => write!(f, "unknown kind: {kind}"),
            VerbError::UnknownInstance { id } => write!(f, "unknown instance: {id}"),
            VerbError::UnknownVerb { verb } => write!(f, "unknown verb: {verb}"),
            VerbError::NotDriver { held_by: Some(p) } => {
                write!(f, "refused: {p} holds the driver seat")
            }
            VerbError::NotDriver { held_by: None } => {
                write!(f, "refused: take the driver seat first (sys.take)")
            }
            VerbError::Denied { why } => write!(f, "denied: {why}"),
            VerbError::BadArgs { why } => write!(f, "bad arguments: {why}"),
            VerbError::Failed { why } => write!(f, "{why}"),
            VerbError::Gone => write!(f, "the instance is gone"),
        }
    }
}

impl std::error::Error for VerbError {}

impl From<CellGone> for VerbError {
    fn from(_: CellGone) -> Self {
        VerbError::Gone
    }
}

/// A live instance's behavior: dispatch one verb. The framework has already
/// authorized the call and routed `sys.*`; implementations see only their
/// own vocabulary. Runs inside the instance's cell — serialized, may await.
#[async_trait::async_trait]
pub trait Instance: Send {
    async fn verb(&mut self, verb: &str, args: Value, cx: &mut Ctx) -> Result<Value, VerbError>;
}

/// A kind: the factory plus the spec. Registered once at startup. `create`
/// receives the cell's [`Signals`](myco_runtime::Signals) up front, so a
/// kind that runs side-feed tasks (a pty reader) wires them inline — there
/// is no second post-construction step.
pub trait Kind: Send + Sync {
    fn spec(&self) -> &'static KindSpec;
    fn create(
        &self,
        args: Value,
        signals: myco_runtime::Signals,
    ) -> Result<Box<dyn Instance>, VerbError>;
}

/// One `sys.log` entry: who called what, and how it went. The acme `event`
/// file reborn — every instance carries its own recent history for free.
#[derive(Debug, Clone, serde::Serialize)]
pub struct VerbLog {
    pub at: chrono::DateTime<chrono::Utc>,
    pub principal: Principal,
    pub verb: String,
    pub ok: bool,
    /// The error's name when not ok (never the payload — logs are cheap to
    /// read, so they must be cheap to expose).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

const VERB_LOG_CAP: usize = 64;

struct Entry {
    id: String,
    kind: &'static KindSpec,
    project: String,
    title: RwLock<String>,
    creator: Principal,
    /// Where `sys.release` returns the seat: the creator.
    default_driver: Principal,
    driver: RwLock<Option<Principal>>,
    log: Mutex<VecDeque<VerbLog>>,
    cell: Cell<Box<dyn Instance>>,
    created_at: chrono::DateTime<chrono::Utc>,
}

/// A listing row — everything the tree UI needs, nothing that requires
/// entering the cell.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct InstanceInfo {
    pub id: String,
    pub kind: String,
    pub project: String,
    pub title: String,
    pub creator: Principal,
    pub driver: Option<Principal>,
    pub watermark: Watermark,
    pub crashed: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// The bus: every kind, every instance, one dispatch surface. L2 exposes
/// exactly this over HTTP; the agent's dispatcher calls it directly. Clone
/// freely.
#[derive(Clone)]
pub struct Pool {
    inner: Arc<PoolInner>,
}

struct PoolInner {
    kinds: RwLock<HashMap<&'static str, Arc<dyn Kind>>>,
    instances: RwLock<HashMap<String, Arc<Entry>>>,
    /// The global feed: lifecycle plus every instance's events, tagged by
    /// id. The tree UI and the debugger both hang off this.
    events: tokio::sync::broadcast::Sender<(String, Event)>,
}

impl Default for Pool {
    fn default() -> Self {
        Self::new()
    }
}

impl Pool {
    pub fn new() -> Self {
        let (events, _) = tokio::sync::broadcast::channel(1024);
        Self {
            inner: Arc::new(PoolInner {
                kinds: RwLock::new(HashMap::new()),
                instances: RwLock::new(HashMap::new()),
                events,
            }),
        }
    }

    /// Register a kind. Startup-time; a duplicate name is a programming
    /// error worth dying loudly for.
    pub fn register(&self, kind: Arc<dyn Kind>) {
        let name = kind.spec().kind;
        let mut kinds = self.inner.kinds.write().unwrap_or_else(|e| e.into_inner());
        assert!(
            kinds.insert(name, kind).is_none(),
            "kind {name:?} registered twice"
        );
    }

    pub fn kinds(&self) -> Vec<&'static KindSpec> {
        self.inner
            .kinds
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|k| k.spec())
            .collect()
    }

    /// Create an instance. The creator starts as driver and remains the
    /// default the seat returns to on release.
    pub fn create(
        &self,
        creator: &Principal,
        kind: &str,
        project: &str,
        title: &str,
        args: Value,
    ) -> Result<InstanceInfo, VerbError> {
        let factory = {
            let kinds = self.inner.kinds.read().unwrap_or_else(|e| e.into_inner());
            kinds
                .get(kind)
                .cloned()
                .ok_or_else(|| VerbError::UnknownKind { kind: kind.into() })?
        };
        let cell = Cell::try_spawn_with(|signals| factory.create(args, signals))?;

        let id = uuid::Uuid::new_v4().to_string();
        let entry = Arc::new(Entry {
            id: id.clone(),
            kind: factory.spec(),
            project: project.to_string(),
            title: RwLock::new(if title.is_empty() {
                factory.spec().kind.to_string()
            } else {
                title.to_string()
            }),
            creator: creator.clone(),
            default_driver: creator.clone(),
            driver: RwLock::new(Some(creator.clone())),
            log: Mutex::new(VecDeque::new()),
            cell,
            created_at: chrono::Utc::now(),
        });

        // Forward the instance's own events onto the global feed, id-tagged.
        {
            let mut rx = entry.cell.subscribe();
            let global = self.inner.events.clone();
            let tag = id.clone();
            tokio::spawn(async move {
                while let Ok(event) = rx.recv().await {
                    let _ = global.send((tag.clone(), event));
                }
            });
        }

        let info = self.info_of(&entry);
        self.inner
            .instances
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.clone(), entry);
        let _ = self.inner.events.send((
            id,
            Event {
                name: "created".into(),
                data: json!({"kind": kind, "project": project}),
            },
        ));
        Ok(info)
    }

    /// Dispatch one verb as `principal`. The framework answers `sys.*`
    /// itself; everything else is authorized against the kind's spec and
    /// applied inside the instance's cell.
    pub async fn call(
        &self,
        principal: &Principal,
        id: &str,
        verb: &str,
        args: Value,
    ) -> Result<Value, VerbError> {
        let entry = self.entry(id)?;
        let result = self.call_inner(principal, &entry, verb, args).await;
        let mut log = entry.log.lock().unwrap_or_else(|e| e.into_inner());
        if log.len() == VERB_LOG_CAP {
            log.pop_front();
        }
        log.push_back(VerbLog {
            at: chrono::Utc::now(),
            principal: principal.clone(),
            verb: verb.to_string(),
            ok: result.is_ok(),
            error: result.as_ref().err().map(error_name),
        });
        result
    }

    async fn call_inner(
        &self,
        principal: &Principal,
        entry: &Arc<Entry>,
        verb: &str,
        args: Value,
    ) -> Result<Value, VerbError> {
        if let Some(rest) = verb.strip_prefix("sys.") {
            return self.sys_verb(principal, entry, rest, args);
        }
        let spec = entry
            .kind
            .verb(verb)
            .ok_or_else(|| VerbError::UnknownVerb { verb: verb.into() })?;
        if spec.requires_driver {
            let driver = entry.driver.read().unwrap_or_else(|e| e.into_inner());
            if driver.as_ref() != Some(principal) {
                return Err(VerbError::NotDriver {
                    held_by: driver.clone(),
                });
            }
        }
        let verb = verb.to_string();
        entry
            .cell
            .call(move |instance, cx| Box::pin(async move { instance.verb(&verb, args, cx).await }))
            .await?
    }

    /// The uniform verbs: introspection and the driver seat.
    fn sys_verb(
        &self,
        principal: &Principal,
        entry: &Arc<Entry>,
        rest: &str,
        args: Value,
    ) -> Result<Value, VerbError> {
        match rest {
            "spec" => Ok(serde_json::to_value(entry.kind).expect("spec serializes")),
            "meta" => Ok(serde_json::to_value(self.info_of(entry)).expect("info serializes")),
            "log" => {
                let limit = args
                    .get("limit")
                    .and_then(Value::as_u64)
                    .map(|n| n as usize)
                    .unwrap_or(VERB_LOG_CAP);
                let log = entry.log.lock().unwrap_or_else(|e| e.into_inner());
                let tail: Vec<&VerbLog> = log.iter().rev().take(limit).collect();
                Ok(serde_json::to_value(&tail).expect("log serializes"))
            }
            "rename" => {
                let title = args.get("title").and_then(Value::as_str).ok_or_else(|| {
                    VerbError::BadArgs {
                        why: "rename needs {title}".into(),
                    }
                })?;
                *entry.title.write().unwrap_or_else(|e| e.into_inner()) = title.to_string();
                let _ = self.inner.events.send((
                    entry.id.clone(),
                    Event {
                        name: "renamed".into(),
                        data: json!({ "title": title }),
                    },
                ));
                Ok(Value::Null)
            }
            "take" => {
                let mut driver = entry.driver.write().unwrap_or_else(|e| e.into_inner());
                let allowed = match (&*driver, principal) {
                    (None, _) => true,
                    (Some(held), p) if held == p => true,
                    // Humans outrank agents; nobody takes from a human, and
                    // agents do not wrestle each other for the seat.
                    (Some(Principal::Agent(_)), Principal::Human(_)) => true,
                    (Some(_), _) => false,
                };
                if !allowed {
                    return Err(VerbError::NotDriver {
                        held_by: driver.clone(),
                    });
                }
                *driver = Some(principal.clone());
                drop(driver);
                self.driver_event(entry);
                Ok(Value::Null)
            }
            "release" => {
                let mut driver = entry.driver.write().unwrap_or_else(|e| e.into_inner());
                if driver.as_ref() != Some(principal) {
                    return Err(VerbError::NotDriver {
                        held_by: driver.clone(),
                    });
                }
                *driver = if *principal == entry.default_driver {
                    None
                } else {
                    Some(entry.default_driver.clone())
                };
                drop(driver);
                self.driver_event(entry);
                Ok(Value::Null)
            }
            other => Err(VerbError::UnknownVerb {
                verb: format!("sys.{other}"),
            }),
        }
    }

    fn driver_event(&self, entry: &Arc<Entry>) {
        let driver = entry
            .driver
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let _ = self.inner.events.send((
            entry.id.clone(),
            Event {
                name: "driver".into(),
                data: serde_json::to_value(&driver).expect("principal serializes"),
            },
        ));
    }

    /// Forget an instance: its cell's last handle drops, queued verbs drain,
    /// the state drops (kinds kill their children there). Creator, current
    /// driver, any human, and system may remove; an agent may remove only
    /// what it created.
    pub fn remove(&self, principal: &Principal, id: &str) -> Result<(), VerbError> {
        let entry = self.entry(id)?;
        let driver = entry
            .driver
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let allowed = match principal {
            Principal::Human(_) | Principal::System(_) => true,
            Principal::Agent(_) => {
                entry.creator == *principal || driver.as_ref() == Some(principal)
            }
        };
        if !allowed {
            return Err(VerbError::Denied {
                why: "an agent may remove only instances it created or drives".into(),
            });
        }
        self.inner
            .instances
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
        let _ = self.inner.events.send((
            id.to_string(),
            Event {
                name: "removed".into(),
                data: Value::Null,
            },
        ));
        Ok(())
    }

    pub fn list(&self, project: Option<&str>) -> Vec<InstanceInfo> {
        let instances = self
            .inner
            .instances
            .read()
            .unwrap_or_else(|e| e.into_inner());
        let mut rows: Vec<InstanceInfo> = instances
            .values()
            .filter(|e| project.is_none_or(|p| e.project == p))
            .map(|e| self.info_of(e))
            .collect();
        rows.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        rows
    }

    pub fn info(&self, id: &str) -> Result<InstanceInfo, VerbError> {
        let entry = self.entry(id)?;
        Ok(self.info_of(&entry))
    }

    /// Current watermark, for starting a watch loop.
    pub fn watermark(&self, id: &str) -> Result<Watermark, VerbError> {
        Ok(self.entry(id)?.cell.watermark())
    }

    /// Wait until the instance's watermark exceeds `since`. Removal or crash
    /// resolves the wait rather than hanging it.
    pub async fn changed(&self, id: &str, since: Watermark) -> Result<Watermark, VerbError> {
        let entry = self.entry(id)?;
        Ok(entry.cell.changed(since).await?)
    }

    /// The global feed: `(instance id, event)` for lifecycle, driver
    /// changes, and every instance's own events.
    pub fn events(&self) -> tokio::sync::broadcast::Receiver<(String, Event)> {
        self.inner.events.subscribe()
    }

    fn entry(&self, id: &str) -> Result<Arc<Entry>, VerbError> {
        self.inner
            .instances
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
            .ok_or_else(|| VerbError::UnknownInstance { id: id.into() })
    }

    fn info_of(&self, entry: &Entry) -> InstanceInfo {
        InstanceInfo {
            id: entry.id.clone(),
            kind: entry.kind.kind.to_string(),
            project: entry.project.clone(),
            title: entry
                .title
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            creator: entry.creator.clone(),
            driver: entry
                .driver
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
            watermark: entry.cell.watermark(),
            crashed: entry.cell.is_crashed(),
            created_at: entry.created_at,
        }
    }
}

fn error_name(e: &VerbError) -> String {
    match e {
        VerbError::UnknownKind { .. } => "unknown_kind",
        VerbError::UnknownInstance { .. } => "unknown_instance",
        VerbError::UnknownVerb { .. } => "unknown_verb",
        VerbError::NotDriver { .. } => "not_driver",
        VerbError::Denied { .. } => "denied",
        VerbError::BadArgs { .. } => "bad_args",
        VerbError::Failed { .. } => "failed",
        VerbError::Gone => "gone",
    }
    .to_string()
}

#[cfg(test)]
mod tests;

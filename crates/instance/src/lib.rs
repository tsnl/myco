//! L1: the instance framework — kinds, verbs, principals, drivers, and the
//! [`Pool`] that fronts them as one bus.
//!
//! Everything is a verb. Reads are verbs (never driver-gated, refused to
//! nobody unless owner-scoped); writes are verbs; driver transfer,
//! introspection, rename, and removal are the `sys.*` verbs the framework
//! answers itself so kinds cannot get them wrong. Authorization is one
//! function of (principal, verb spec, entry state), enforced by refusal —
//! never by blocking — and for driver-gated verbs it is enforced **twice**:
//! once at enqueue (fast refusal, ahead of the queue) and again at apply
//! (inside the mailbox, so authority is bound to effect: a `sys.take`
//! fences every not-yet-applied verb from the old driver).
//!
//! The driver is authority, not a mutex: durable, visible state on the
//! instance, changed by `sys.take`/`sys.release` under one policy — humans
//! may take from agents or from nobody; an agent may never take from a
//! human; release returns the instance to its creator, or to nobody when
//! the creator releases. `owner_only` is the second, immutable authority
//! axis: a verb scoped to the instance's creator (a per-user inbox's reads,
//! its ack), checkable before the queue because the creator never changes.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, RwLock};

use myco_runtime::{Cell, CellGone, Event, Signals, Watermark};
use serde_json::{Value, json};

pub mod events;
mod shared;

pub use shared::Shared;

/// Who is asking. An agent is named by the chat instance it drives — both
/// adapters (HTTP for humans, the model loop for agents) resolve to this
/// before anything touches the bus.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
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

/// One verb in a kind's vocabulary. The flags are schema — promises to
/// consumers, some enforced by the framework (`requires_driver`,
/// `owner_only`), some contracts the kind must honor (`read_only` purity,
/// `cursored` delta-reads for M2's standing subscriptions).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct VerbSpec {
    pub name: &'static str,
    pub doc: &'static str,
    /// Pure: no state change. A schema promise (the framework cannot verify
    /// purity), consumed by clients that call reads speculatively.
    pub read_only: bool,
    /// Only the current driver may call it. Enforced at enqueue and again
    /// at apply.
    pub requires_driver: bool,
    /// Only the instance's creator may call it. Enforced at enqueue; the
    /// creator is immutable, so no apply-time re-check is needed.
    pub owner_only: bool,
    /// Takes a cursor and returns the next one, so callers read deltas.
    /// Consumer: an agent's standing subscriptions (M2).
    pub cursored: bool,
}

impl VerbSpec {
    pub const fn read(name: &'static str, doc: &'static str) -> Self {
        Self {
            name,
            doc,
            read_only: true,
            requires_driver: false,
            owner_only: false,
            cursored: false,
        }
    }

    pub const fn cursored_read(name: &'static str, doc: &'static str) -> Self {
        Self {
            read_only: true,
            cursored: true,
            ..Self::read(name, doc)
        }
    }

    pub const fn driven(name: &'static str, doc: &'static str) -> Self {
        Self {
            read_only: false,
            requires_driver: true,
            ..Self::read(name, doc)
        }
    }

    pub const fn write(name: &'static str, doc: &'static str) -> Self {
        Self {
            read_only: false,
            ..Self::read(name, doc)
        }
    }

    /// A write only the creator may perform (a private instance's `ack`).
    pub const fn owned(name: &'static str, doc: &'static str) -> Self {
        Self {
            read_only: false,
            owner_only: true,
            ..Self::read(name, doc)
        }
    }

    /// A read only the creator may perform (a private instance's state).
    pub const fn owned_read(name: &'static str, doc: &'static str) -> Self {
        Self {
            owner_only: true,
            ..Self::read(name, doc)
        }
    }

    /// A cursored read only the creator may perform (a private inbox).
    pub const fn owned_cursored_read(name: &'static str, doc: &'static str) -> Self {
        Self {
            cursored: true,
            ..Self::owned_read(name, doc)
        }
    }
}

/// A kind's contract: its verb vocabulary plus the two hints consumers use
/// to pick a default read — `primary_render` for pane renderers,
/// `recommended_context` for an agent's context assembly. Every kind must
/// have at least one plain read, and each hint must name a real verb —
/// [`Pool::register`] asserts both. The escape-hatch rule: the
/// `recommended_context` verb's payload must contain a canonical plain-text
/// field (see DESIGN.md), so every instance stays greppable.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct KindSpec {
    pub kind: &'static str,
    /// Schema version, serialized into `sys.spec` — bump on any change to
    /// verb names, argument shapes, or payloads once protocol providers
    /// (M4) can be older than the pool.
    pub version: u32,
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

/// A title is a slug: `^[A-Za-z0-9][A-Za-z0-9-]*$`. Empty at create
/// becomes the kind name (which must itself be a slug).
fn valid_title(title: &str) -> bool {
    let mut bytes = title.bytes();
    match bytes.next() {
        Some(b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9') => {
            bytes.all(|c| matches!(c, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-'))
        }
        _ => false,
    }
}

fn resolve_title(title: &str, kind: &str) -> Result<String, VerbError> {
    let resolved = if title.is_empty() {
        kind.to_string()
    } else {
        title.to_string()
    };
    if !valid_title(&resolved) {
        return Err(VerbError::BadArgs {
            why: format!("title {resolved:?} must match ^[A-Za-z0-9][A-Za-z0-9-]*$"),
        });
    }
    Ok(resolved)
}

fn title_taken(
    instances: &HashMap<String, Arc<Entry>>,
    project: &str,
    title: &str,
    except: Option<&str>,
) -> Option<String> {
    instances.values().find_map(|e| {
        if except == Some(e.id.as_str()) {
            return None;
        }
        let same =
            e.project == project && *e.title.read().unwrap_or_else(|e| e.into_inner()) == title;
        same.then(|| e.id.clone())
    })
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
///
/// `caller` is the authenticated principal the framework already checked —
/// attribution as a framework fact, so an attributed kind (a chat recording
/// who posted) can never be lied to by its own arguments. Kinds that don't
/// attribute simply ignore it.
#[async_trait::async_trait]
pub trait Instance: Send {
    async fn verb(
        &mut self,
        caller: &Principal,
        verb: &str,
        args: Value,
        signals: &Signals,
    ) -> Result<Value, VerbError>;
}

/// What the framework knows about an instance at birth, handed to the
/// factory. `id` lets a kind that acts on the bus speak as
/// `Principal::Agent(id)`; `creator` lets a per-principal kind (a
/// notifier) bind to its owner as a framework fact — args would be
/// forgeable. Kinds that need none of it ignore it.
///
/// Every field here is *identity*: fixed at this instant and never edited
/// afterwards, which is what makes it safe for a kind to copy.
#[derive(Debug, Clone)]
pub struct CreateCtx {
    pub id: String,
    pub creator: Principal,
    /// The instance this one was created under, if any. The same value the
    /// listing carries — a kind that wants it should read it here rather
    /// than accept it in args, where a caller could name anything.
    pub parent: Option<String>,
}

/// A kind: the factory plus the spec. Registered once at startup. `create`
/// receives the cell's [`Signals`] up front, so a kind that runs side-feed
/// tasks (a pty reader) wires them inline.
///
/// Kinds that consume the bus itself (an event-fed notifier) hold a
/// [`Pool`] clone as a factory field — blessed dependency injection, with
/// three rules that keep it safe:
/// - side-feed tasks may call the pool freely;
/// - a verb handler must never call a verb on its own instance (the
///   framework refuses the direct case — but the refusal is task-scoped:
///   a task *spawned inside* a handler escapes it, so a handler must
///   never await verb results from tasks it spawns; hand that work to a
///   side-feed, fire-and-forget) nor on instances that may call back —
///   call cycles deadlock, undetectably past one hop;
/// - a side-feed whose input does not die with the instance (a pool event
///   feed, unlike a pty) must be cancelled by the instance's `Drop`.
pub trait Kind: Send + Sync {
    fn spec(&self) -> &'static KindSpec;
    fn create(
        &self,
        ctx: &CreateCtx,
        args: Value,
        signals: Signals,
    ) -> Result<Box<dyn Instance>, VerbError>;
}

/// One `sys.log` entry: who called what, and how it went. The acme `event`
/// file reborn — every instance carries its own recent history for free.
/// Introspection reads (`sys.spec`/`sys.meta`/`sys.log`) are not recorded,
/// so watching the debugger does not erase the evidence.
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
    /// Immutable; doubles as the owner (`owner_only` verbs) and the default
    /// driver the seat returns to on release.
    creator: Principal,
    /// The instance this one was created under, if any. Immutable, like the
    /// creator, and for the same reason: identity is not state.
    parent: Option<String>,
    /// Shared with apply-time closures, so authority is re-checked when a
    /// queued verb actually runs — not just when it was admitted.
    driver: Arc<RwLock<Option<Principal>>>,
    log: Mutex<VecDeque<VerbLog>>,
    cell: Cell<Box<dyn Instance>>,
    created_at: chrono::DateTime<chrono::Utc>,
}

/// A listing row — everything the tree UI needs, nothing that requires
/// entering the cell (a wedged instance must not wedge the tree).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InstanceInfo {
    pub id: String,
    pub kind: String,
    pub project: String,
    pub title: String,
    pub creator: Principal,
    /// Who this instance was created under — a subagent chat names the chat
    /// that spawned it. Set at birth, never changed, and a soft reference:
    /// a parent may be removed while its children live on, so a consumer
    /// that cannot resolve it treats the row as a root.
    #[serde(default)]
    pub parent: Option<String>,
    pub driver: Option<Principal>,
    pub watermark: Watermark,
    /// Eventually-true after a kind panic. `false` means "not known dead" —
    /// a `Gone` from a verb call is the definitive signal.
    pub crashed: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

tokio::task_local! {
    /// The instance id whose verb handler is currently applying on this
    /// task — the re-entrancy tripwire: a handler calling back into its own
    /// instance would deadlock the mailbox, so the dispatch refuses it.
    static APPLYING: String;
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
    /// id. Best-effort by design — a lagging consumer skips and recovers by
    /// re-reading state (`list`, watermarks), never by trusting the feed to
    /// be complete. Not causally ordered across instances or with removal:
    /// a side-feed's last words ("exited") can trail "removed".
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

    /// Register a kind. Startup-time; a malformed spec is a programming
    /// error worth dying loudly for, so the contract is asserted here:
    /// both hints name real verbs, at least one plain read exists, and no
    /// verb claims to be a driver-gated read.
    pub fn register(&self, kind: Arc<dyn Kind>) {
        let spec = kind.spec();
        let name = spec.kind;
        assert!(
            spec.verb(spec.primary_render).is_some(),
            "kind {name:?}: primary_render {:?} is not a verb",
            spec.primary_render
        );
        assert!(
            spec.verb(spec.recommended_context).is_some(),
            "kind {name:?}: recommended_context {:?} is not a verb",
            spec.recommended_context
        );
        assert!(
            spec.verbs.iter().any(|v| v.read_only),
            "kind {name:?} declares no read verb"
        );
        for v in spec.verbs {
            assert!(
                !(v.read_only && v.requires_driver),
                "kind {name:?}: verb {:?} cannot be a driver-gated read",
                v.name
            );
        }
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

    /// Create an instance. The creator becomes owner (immutable), first
    /// driver, and the seat's home on release.
    pub fn create(
        &self,
        creator: &Principal,
        kind: &str,
        project: &str,
        title: &str,
        args: Value,
    ) -> Result<InstanceInfo, VerbError> {
        self.create_under(creator, kind, project, title, args, None)
    }

    /// Create an instance under a parent — the chat a subagent was spawned
    /// from. Parentage is L1 identity, not kind state: fixed at birth,
    /// never edited, and present in every listing row, so "what spawned
    /// this" is a framework fact no kind has to model and no consumer has
    /// to reconstruct by reading kind-specific verbs.
    ///
    /// The parent must exist *now* — a birth certificate naming nobody is a
    /// bug worth refusing — but nothing pins it alive afterwards.
    pub fn create_under(
        &self,
        creator: &Principal,
        kind: &str,
        project: &str,
        title: &str,
        args: Value,
        parent: Option<&str>,
    ) -> Result<InstanceInfo, VerbError> {
        if let Some(parent) = parent {
            let known = self
                .inner
                .instances
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .contains_key(parent);
            if !known {
                return Err(VerbError::UnknownInstance { id: parent.into() });
            }
        }
        let factory = {
            let kinds = self.inner.kinds.read().unwrap_or_else(|e| e.into_inner());
            kinds
                .get(kind)
                .cloned()
                .ok_or_else(|| VerbError::UnknownKind { kind: kind.into() })?
        };
        let title = resolve_title(title, factory.spec().kind)?;
        {
            let instances = self
                .inner
                .instances
                .read()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(other) = title_taken(&instances, project, &title, None) {
                return Err(VerbError::BadArgs {
                    why: format!("title {title:?} is already used by {other}"),
                });
            }
        }
        // The context exists before the instance does, so `create` can hand
        // its kind a usable self-name and owner.
        let ctx = CreateCtx {
            id: uuid::Uuid::new_v4().to_string(),
            creator: creator.clone(),
            parent: parent.map(str::to_string),
        };
        let id = ctx.id.clone();
        // Subscribe inside the builder, before `create` can spawn side-feed
        // tasks: nothing the instance ever emits predates the subscription.
        let mut cell_events = None;
        let cell = Cell::try_spawn_with(|signals| {
            cell_events = Some(signals.subscribe());
            factory.create(&ctx, args, signals)
        })?;
        let mut rx = cell_events.expect("builder ran");
        let entry = Arc::new(Entry {
            id: id.clone(),
            kind: factory.spec(),
            project: project.to_string(),
            title: RwLock::new(title.clone()),
            creator: creator.clone(),
            parent: ctx.parent.clone(),
            driver: Arc::new(RwLock::new(Some(creator.clone()))),
            log: Mutex::new(VecDeque::new()),
            cell,
            created_at: chrono::Utc::now(),
        });

        // Forward the instance's own events onto the global feed, id-tagged.
        // Lag is survivable (skip and continue); only closure ends the task.
        {
            let global = self.inner.events.clone();
            let tag = id.clone();
            tokio::spawn(async move {
                loop {
                    match rx.recv().await {
                        Ok(event) => {
                            let _ = global.send((tag.clone(), event));
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }

        let info = self.info_of(&entry);
        {
            let mut instances = self
                .inner
                .instances
                .write()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(other) = title_taken(&instances, project, &title, None) {
                return Err(VerbError::BadArgs {
                    why: format!("title {title:?} is already used by {other}"),
                });
            }
            instances.insert(id.clone(), entry);
        }
        let _ = self.inner.events.send((
            id,
            Event {
                name: "created".into(),
                data: json!({
                    "kind": kind,
                    "project": project,
                    "creator": creator,
                    "parent": parent,
                }),
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
        // Introspection is free: reading the debugger must not erase the
        // evidence it exists to keep.
        if !matches!(verb, "sys.spec" | "sys.meta" | "sys.log") {
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
        }
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
        if spec.owner_only && entry.creator != *principal {
            return Err(VerbError::Denied {
                why: format!("owner only ({})", entry.creator),
            });
        }
        // Enqueue-time check: refuse ahead of the queue, so a non-driver
        // never waits behind the very verbs it should have been refused
        // before.
        if spec.requires_driver {
            let driver = entry.driver.read().unwrap_or_else(|e| e.into_inner());
            if driver.as_ref() != Some(principal) {
                return Err(VerbError::NotDriver {
                    held_by: driver.clone(),
                });
            }
        }
        // A verb handler calling back into its own instance would wait on a
        // reply only its own return can produce. Refuse, don't deadlock.
        let self_call = APPLYING.try_with(|cur| *cur == entry.id).unwrap_or(false);
        if self_call {
            return Err(VerbError::Denied {
                why: "re-entrant call: a verb handler may not call its own instance".into(),
            });
        }

        let verb_name = verb.to_string();
        let requires_driver = spec.requires_driver;
        let driver_slot = Arc::clone(&entry.driver);
        let caller = principal.clone();
        let applying_id = entry.id.clone();
        entry
            .cell
            .call(move |instance, signals| {
                Box::pin(APPLYING.scope(applying_id, async move {
                    // Apply-time re-check binds authority to effect: a
                    // sys.take fences every verb the old driver had queued
                    // but not yet landed. (Synchronous read; the guard drops
                    // before any await.)
                    if requires_driver {
                        let held = driver_slot
                            .read()
                            .unwrap_or_else(|e| e.into_inner())
                            .clone();
                        if held.as_ref() != Some(&caller) {
                            return Err(VerbError::NotDriver { held_by: held });
                        }
                    }
                    instance.verb(&caller, &verb_name, args, signals).await
                }))
            })
            .await?
    }

    /// The uniform verbs: introspection, identity, the driver seat, and
    /// removal — entry/pool-level, never entering the mailbox, so they work
    /// on a wedged or crashed instance (that is the rescue path).
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
                if title.is_empty() || !valid_title(title) {
                    return Err(VerbError::BadArgs {
                        why: format!("title {title:?} must match ^[A-Za-z0-9][A-Za-z0-9-]*$"),
                    });
                }
                {
                    let instances = self
                        .inner
                        .instances
                        .write()
                        .unwrap_or_else(|e| e.into_inner());
                    if let Some(other) =
                        title_taken(&instances, &entry.project, title, Some(&entry.id))
                    {
                        return Err(VerbError::BadArgs {
                            why: format!("title {title:?} is already used by {other}"),
                        });
                    }
                    *entry.title.write().unwrap_or_else(|e| e.into_inner()) = title.to_string();
                }
                self.meta_changed(entry, "renamed", json!({ "title": title }));
                Ok(Value::Null)
            }
            "take" => {
                let mut driver = entry.driver.write().unwrap_or_else(|e| e.into_inner());
                let allowed = match (&*driver, principal) {
                    (None, _) => true,
                    (Some(held), p) if held == p => true,
                    // Humans outrank agents; nobody takes an occupied seat
                    // otherwise — not from a human, not System, and agents
                    // do not wrestle each other for it.
                    (Some(Principal::Agent(_)), Principal::Human(_)) => true,
                    (Some(_), _) => false,
                };
                if !allowed {
                    return Err(VerbError::NotDriver {
                        held_by: driver.clone(),
                    });
                }
                *driver = Some(principal.clone());
                let now = driver.clone();
                drop(driver);
                self.meta_changed(
                    entry,
                    "driver",
                    serde_json::to_value(&now).expect("principal"),
                );
                Ok(Value::Null)
            }
            "release" => {
                let mut driver = entry.driver.write().unwrap_or_else(|e| e.into_inner());
                if driver.as_ref() != Some(principal) {
                    return Err(VerbError::NotDriver {
                        held_by: driver.clone(),
                    });
                }
                *driver = if *principal == entry.creator {
                    None
                } else {
                    Some(entry.creator.clone())
                };
                let now = driver.clone();
                drop(driver);
                self.meta_changed(
                    entry,
                    "driver",
                    serde_json::to_value(&now).expect("principal"),
                );
                Ok(Value::Null)
            }
            "remove" => {
                // Humans and system may remove anything; an agent only what
                // it created or currently drives.
                let allowed = match principal {
                    Principal::Human(_) | Principal::System(_) => true,
                    Principal::Agent(_) => {
                        let driving = entry
                            .driver
                            .read()
                            .unwrap_or_else(|e| e.into_inner())
                            .as_ref()
                            == Some(principal);
                        entry.creator == *principal || driving
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
                    .remove(&entry.id);
                let _ = self.inner.events.send((
                    entry.id.clone(),
                    Event {
                        name: "removed".into(),
                        data: Value::Null,
                    },
                ));
                // Wake watermark watchers so a `changed` wait resolves; the
                // next call answers UnknownInstance. (The cell itself ends
                // when its last handle drops — the state drop is where kinds
                // kill their children.)
                entry.cell.signals().bump();
                Ok(Value::Null)
            }
            other => Err(VerbError::UnknownVerb {
                verb: format!("sys.{other}"),
            }),
        }
    }

    /// Meta changes (driver, title) are observable state: they travel the
    /// event feed as history *and* bump the watermark so meta watchers
    /// re-read — the same contract kind-state changes honor.
    fn meta_changed(&self, entry: &Arc<Entry>, event: &str, data: Value) {
        let _ = self.inner.events.send((
            entry.id.clone(),
            Event {
                name: event.into(),
                data,
            },
        ));
        entry.cell.signals().bump();
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

    /// The chain of parents above `id`, nearest first — for the depth caps
    /// and nesting views that would otherwise re-walk kind verbs to learn
    /// what L1 already knows. The walk always terminates: a parent must
    /// exist before its child, so the graph cannot contain a cycle. A
    /// removed parent still appears (its children still name it) and ends
    /// the chain there, since nothing remains to say what stood above it.
    pub fn ancestors(&self, id: &str) -> Vec<String> {
        let instances = self
            .inner
            .instances
            .read()
            .unwrap_or_else(|e| e.into_inner());
        let mut chain = Vec::new();
        let mut next = instances.get(id).and_then(|e| e.parent.clone());
        while let Some(parent) = next {
            next = instances.get(&parent).and_then(|e| e.parent.clone());
            chain.push(parent);
        }
        chain
    }

    /// Current watermark, for starting a watch loop.
    pub fn watermark(&self, id: &str) -> Result<Watermark, VerbError> {
        Ok(self.entry(id)?.cell.watermark())
    }

    /// Wait until the instance's watermark exceeds `since`. Removal and
    /// crash both bump, so the wait resolves rather than hanging; the next
    /// call reports what happened.
    pub async fn changed(&self, id: &str, since: Watermark) -> Result<Watermark, VerbError> {
        let entry = self.entry(id)?;
        Ok(entry.cell.changed(since).await?)
    }

    /// Read `verb` until the answer satisfies `settled`, waking on the
    /// watermark. Answers `None` when `deadline` passes first.
    ///
    /// The wait-for-a-condition shape, once: read, check, wait for a
    /// change, read again — never a sleep, because a sleep is either a
    /// stall or a race and usually both. Callers that hand-rolled it kept
    /// rediscovering the same two rules: check *before* the first wait (the
    /// thing may already be true), and let a bumped watermark, not a timer,
    /// decide when to look again.
    pub async fn wait_until(
        &self,
        principal: &Principal,
        id: &str,
        verb: &str,
        deadline: tokio::time::Instant,
        settled: impl Fn(&Value) -> bool,
    ) -> Result<Option<Value>, VerbError> {
        let mut mark = 0;
        loop {
            let seen = self.call(principal, id, verb, Value::Null).await?;
            if settled(&seen) {
                return Ok(Some(seen));
            }
            match tokio::time::timeout_at(deadline, self.changed(id, mark)).await {
                Ok(Ok(m)) => mark = m,
                Ok(Err(e)) => return Err(e),
                Err(_) => return Ok(None),
            }
        }
    }

    /// The global feed: `(instance id, event)` for lifecycle, meta changes,
    /// and every instance's own events. Best-effort — see [`PoolInner`].
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
            parent: entry.parent.clone(),
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

/// The error's wire name, taken from its own serde tag so the verb log and
/// the serialized error can never disagree.
fn error_name(e: &VerbError) -> String {
    serde_json::to_value(e)
        .ok()
        .and_then(|v| v.get("error").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(test)]
mod tests;

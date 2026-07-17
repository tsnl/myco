//! Server state: thin HTTP adapter over the shared [`Application`] core.
//!
//! Session lifecycle, model construction, system prompt, and the
//! `session_meta` / `session_history` root tools all live in
//! [`crate::application`]. This module only owns:
//! - a per-session broadcast bus (SSE fan-out) injected as the agent's
//!   [`EventSink`] at open time;
//! - the optional static GUI `dist_dir`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::sync::broadcast;

use crate::application::{Application, LiveSession};
use crate::generative_model::{Effort, Model};
use crate::session::{AgentEvent, EventSink, list_sessions};

use super::wire::WireEvent;

/// Broadcast buffer per session. Slow SSE clients that lag beyond this miss
/// events (they receive `Lagged` and resync from the next event); the canonical
/// transcript is always refetchable via `GET /api/sessions/{id}`.
const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// [`EventSink`] that fans runtime events out to SSE subscribers as [`WireEvent`]s.
pub struct BroadcastSink {
    tx: broadcast::Sender<WireEvent>,
}

impl BroadcastSink {
    fn new(tx: broadcast::Sender<WireEvent>) -> Self {
        Self { tx }
    }
}

impl EventSink for BroadcastSink {
    fn emit(&self, event: AgentEvent) {
        // Err only when there are no subscribers; that is fine (fire and forget).
        let _ = self.tx.send(WireEvent::from(&event));
    }
}

/// Shared, cloneable application state handed to every rocket route.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<AppStateInner>,
}

struct AppStateInner {
    app: Application,
    /// Per-session SSE buses, created when a session is first opened/created
    /// here (paired with the `BroadcastSink` injected into the agent).
    buses: Mutex<HashMap<String, broadcast::Sender<WireEvent>>>,
    /// When set, serve the built GUI (`index.html` + assets) from this dir.
    /// `None` in dev (Trunk serves the client and proxies `/api` here).
    dist_dir: Option<PathBuf>,
}

impl AppState {
    pub fn new(app: Application, dist_dir: Option<PathBuf>) -> Self {
        Self {
            inner: Arc::new(AppStateInner {
                app,
                buses: Mutex::new(HashMap::new()),
                dist_dir,
            }),
        }
    }

    pub fn app(&self) -> &Application {
        &self.inner.app
    }

    /// Directory of built GUI assets, if production static serving is enabled.
    pub fn dist_dir(&self) -> Option<PathBuf> {
        self.inner.dist_dir.clone()
    }

    pub fn live_summaries(&self) -> Vec<(String, String, Option<String>)> {
        self.inner.app.live_summaries()
    }

    pub fn get_live(&self, id: &str) -> Option<Arc<LiveSession>> {
        self.inner.app.get_live(id)
    }

    /// Create a brand-new session and register it as live with an SSE bus.
    pub fn create_session(
        &self,
        model: Model,
        effort: Option<Effort>,
    ) -> Result<Arc<LiveSession>, String> {
        let (tx, _rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let sink = Arc::new(BroadcastSink::new(tx.clone())) as Arc<dyn EventSink>;
        let live = self.inner.app.create_session(model, effort, sink)?;
        self.insert_bus(&live.active.id(), tx);
        Ok(live)
    }

    /// Open a saved session by id/prefix (or return it if already live).
    pub fn open_session(&self, id_or_prefix: &str) -> Result<Arc<LiveSession>, String> {
        if let Some(existing) = self.inner.app.get_live(id_or_prefix) {
            return Ok(existing);
        }
        // Create the bus + sink only when Application will build a new agent.
        let (tx, _rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let sink = Arc::new(BroadcastSink::new(tx.clone())) as Arc<dyn EventSink>;
        let live = self.inner.app.open_session(id_or_prefix, sink)?;
        // If open_session returned an already-live session, do not overwrite
        // its bus (the sink we just built was unused).
        let id = live.active.id();
        let mut buses = self.lock_buses();
        buses.entry(id).or_insert(tx);
        Ok(live)
    }

    /// Subscribe to the SSE bus for a live session.
    pub fn subscribe(&self, id: &str) -> Option<broadcast::Receiver<WireEvent>> {
        self.lock_buses().get(id).map(|tx| tx.subscribe())
    }

    /// Push a non-[`AgentEvent`] server-side frame (turn error) onto the bus.
    pub fn emit_wire(&self, id: &str, event: WireEvent) {
        if let Some(tx) = self.lock_buses().get(id) {
            let _ = tx.send(event);
        }
    }

    /// Saved sessions on disk (id, updated_at desc, capped by `limit`; 0 = all).
    pub fn saved_sessions(
        &self,
        limit: usize,
    ) -> Result<Vec<crate::session::SessionListEntry>, String> {
        list_sessions(limit)
    }

    fn insert_bus(&self, id: &str, tx: broadcast::Sender<WireEvent>) {
        self.lock_buses().insert(id.to_string(), tx);
    }

    fn lock_buses(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<String, broadcast::Sender<WireEvent>>> {
        self.inner.buses.lock().unwrap_or_else(|e| e.into_inner())
    }
}

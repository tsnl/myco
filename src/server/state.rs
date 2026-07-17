//! Server state: a registry of live sessions over a shared [`Harness`].
//!
//! Each open session owns an [`Agent`], a broadcast channel of [`WireEvent`]s
//! (fanned out to any number of SSE subscribers), and a [`CancelToken`] for the
//! in-flight turn. The `Harness` and model config are shared across sessions —
//! hosts already fan out concurrently, so N live agents share one host pool.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::{Mutex as AsyncMutex, broadcast};

use crate::generative_model::{self, BackendConfig, Effort, GenerativeModelConfig, Model};
use crate::harness::Harness;
use crate::session::{
    ActiveSession, Agent, AgentEvent, EventSink, Session, TraceContext, list_sessions,
};

use super::wire::WireEvent;

/// System-prompt prologue (kept in sync with the CLI entry point).
const SYSTEM_PROMPT_PROLOGUE: &str = r#"
You are a helpful assistant running in an agentic harness with unfettered computer access.
"#;

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

/// One live, open session: the agent, its metadata handle, and its event bus.
pub struct LiveSession {
    /// Serialized turns: only one `interact` runs at a time per session.
    pub agent: AsyncMutex<Agent>,
    /// Shared session document (title/scratchpad/links + persisted messages).
    pub active: ActiveSession,
    /// Fan-out bus for live events (subscribe per SSE connection).
    pub events: broadcast::Sender<WireEvent>,
    /// Cancel handle for the turn currently in flight (if any).
    pub cancel: Mutex<Option<crate::CancelToken>>,
    /// Model id backing this session's agent.
    pub model: Model,
}

impl LiveSession {
    pub fn subscribe(&self) -> broadcast::Receiver<WireEvent> {
        self.events.subscribe()
    }
}

/// Shared, cloneable application state handed to every rocket route.
#[derive(Clone)]
pub struct AppState {
    inner: Arc<AppStateInner>,
}

struct AppStateInner {
    harness: Arc<Harness>,
    /// Open sessions keyed by session id.
    live: Mutex<HashMap<String, Arc<LiveSession>>>,
    /// Default reasoning effort for newly created sessions.
    default_effort: Effort,
    debug_dump_api_requests: bool,
    /// When set, serve the built GUI (`index.html` + assets) from this dir.
    /// `None` in dev (Trunk serves the client and proxies `/api` here).
    dist_dir: Option<std::path::PathBuf>,
}

impl AppState {
    pub fn new(
        harness: Arc<Harness>,
        default_effort: Effort,
        debug_dump_api_requests: bool,
        dist_dir: Option<std::path::PathBuf>,
    ) -> Self {
        Self {
            inner: Arc::new(AppStateInner {
                harness,
                live: Mutex::new(HashMap::new()),
                default_effort,
                debug_dump_api_requests,
                dist_dir,
            }),
        }
    }

    pub fn harness(&self) -> &Arc<Harness> {
        &self.inner.harness
    }

    /// Directory of built GUI assets, if production static serving is enabled.
    pub fn dist_dir(&self) -> Option<std::path::PathBuf> {
        self.inner.dist_dir.clone()
    }

    /// Currently open (in-memory) sessions, most-recent activity first is not
    /// guaranteed here; callers sort. Returns `(id, model, title)` snapshots.
    pub fn live_summaries(&self) -> Vec<(String, String, Option<String>)> {
        let live = self.lock_live();
        live.values()
            .map(|s| {
                let snap = s.active.snapshot();
                (snap.id, snap.model, snap.title)
            })
            .collect()
    }

    pub fn get_live(&self, id: &str) -> Option<Arc<LiveSession>> {
        self.lock_live().get(id).cloned()
    }

    /// Create a brand-new session (not yet persisted until its first turn) and
    /// register it as live. Returns the live handle.
    pub fn create_session(
        &self,
        model: Model,
        effort: Option<Effort>,
    ) -> Result<Arc<LiveSession>, String> {
        let session = Session::new(model);
        self.register(session, model, effort.unwrap_or(self.inner.default_effort))
    }

    /// Open a saved session by id/prefix (or return it if already live).
    pub fn open_session(&self, id_or_prefix: &str) -> Result<Arc<LiveSession>, String> {
        // Fast path: already live by exact id.
        if let Some(existing) = self.get_live(id_or_prefix) {
            return Ok(existing);
        }
        let session = Session::load_by_id_or_prefix(id_or_prefix)?;
        if let Some(existing) = self.get_live(&session.id) {
            return Ok(existing);
        }
        let model: Model = session.model.parse().map_err(|e| {
            format!(
                "session {} has unknown model {:?}: {e}",
                session.id, session.model
            )
        })?;
        let effort = self.inner.default_effort;
        self.register(session, model, effort)
    }

    /// Build the agent + event bus for `session` and insert into the live map.
    fn register(
        &self,
        session: Session,
        model: Model,
        effort: Effort,
    ) -> Result<Arc<LiveSession>, String> {
        let id = session.id.clone();
        let messages = session.messages.clone();
        let active = ActiveSession::new(session);

        let (tx, _rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let sink = Arc::new(BroadcastSink::new(tx.clone())) as Arc<dyn EventSink>;

        let model_impl = self.build_model(model, effort)?;
        let mut agent = Agent::with_context(
            model_impl,
            self.inner.harness.clone(),
            sink,
            TraceContext::root(),
        );
        agent.set_history(messages);

        let live = Arc::new(LiveSession {
            agent: AsyncMutex::new(agent),
            active,
            events: tx,
            cancel: Mutex::new(None),
            model,
        });
        self.lock_live().insert(id, live.clone());
        Ok(live)
    }

    fn build_model(
        &self,
        model_id: Model,
        effort: Effort,
    ) -> Result<Arc<dyn generative_model::GenerativeModel>, String> {
        let mut backend_config = BackendConfig::default_for_model(model_id);
        match &mut backend_config {
            BackendConfig::Anthropic(c) => {
                c.debug_dump_api_requests = self.inner.debug_dump_api_requests;
                c.effort = Some(effort);
            }
            BackendConfig::OpenAIResponses(c) => {
                c.debug_dump_api_requests = self.inner.debug_dump_api_requests;
                c.effort = Some(effort);
            }
        }
        generative_model::new(GenerativeModelConfig {
            model: model_id,
            tools: self.inner.harness.tool_specs(),
            system_prompt: [
                SYSTEM_PROMPT_PROLOGUE,
                crate::prompts::DEFAULT_AGENT_PROMPT_EPILOGUE,
            ]
            .join("\n"),
            backend_config: Some(backend_config),
        })
        .map_err(|e| format!("failed to create model {model_id}: {e}"))
    }

    /// Saved sessions on disk (id, updated_at desc, capped by `limit`; 0 = all).
    pub fn saved_sessions(
        &self,
        limit: usize,
    ) -> Result<Vec<crate::session::SessionListEntry>, String> {
        list_sessions(limit)
    }

    fn lock_live(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<LiveSession>>> {
        self.inner.live.lock().unwrap_or_else(|e| e.into_inner())
    }
}

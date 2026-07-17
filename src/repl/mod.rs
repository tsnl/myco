//! Transport-agnostic REPL core.
//!
//! Owns the shared [`Harness`], model/effort configuration, a multi-session
//! registry, and the single source of truth for the system-prompt prologue and
//! model construction. Front-ends (CLI, HTTP server) are thin adapters over
//! this type: the GUI is the general multi-session case; the CLI drives one
//! live session at a time.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

use crate::CancelToken;
use crate::generative_model::{
    self, BackendConfig, Effort, GenerativeModel, GenerativeModelConfig, Model,
};
use crate::harness::{Harness, HarnessConfig};
use crate::prompts;
use crate::session::{ActiveSession, Agent, EventSink, Session, TraceContext};
use crate::tool_services::{
    ActiveSessionResolver, SessionHistoryTool, SessionMetaTool, ToolService,
};

/// System-prompt prologue shared by every root agent (CLI, server, …).
pub const SYSTEM_PROMPT_PROLOGUE: &str = r#"
You are a helpful assistant running in an agentic harness with unfettered computer access.
"#;

/// Build a generative model wired to `harness` tools and the shared system prompt.
pub fn build_model(
    model_id: Model,
    harness: &Harness,
    debug_dump_api_requests: bool,
    effort: Effort,
) -> Result<Arc<dyn GenerativeModel>, String> {
    let mut backend_config = BackendConfig::default_for_model(model_id);
    match &mut backend_config {
        BackendConfig::Anthropic(c) => {
            c.debug_dump_api_requests = debug_dump_api_requests;
            c.effort = Some(effort);
        }
        BackendConfig::OpenAIResponses(c) => {
            c.debug_dump_api_requests = debug_dump_api_requests;
            c.effort = Some(effort);
        }
    }

    generative_model::new(GenerativeModelConfig {
        model: model_id,
        tools: harness.tool_specs(),
        system_prompt: [
            SYSTEM_PROMPT_PROLOGUE,
            prompts::DEFAULT_AGENT_PROMPT_EPILOGUE,
        ]
        .join("\n"),
        backend_config: Some(backend_config),
    })
    .map_err(|e| format!("failed to create model {model_id}: {e}"))
}

// ---------------------------------------------------------------------------
// Session registry (SessionMetaTool resolution)
// ---------------------------------------------------------------------------

/// Maps root `agent_id`s to their [`ActiveSession`] and supports sole-session
/// fallback so subagents (unregistered agent ids) resolve to the single live
/// session — matching CLI behavior where `session_meta` always edits the root.
#[derive(Default)]
pub struct SessionRegistry {
    inner: Mutex<RegistryInner>,
}

#[derive(Default)]
struct RegistryInner {
    /// Root agent_id → active session handle.
    by_agent: HashMap<Uuid, ActiveSession>,
    /// All registered sessions (deduped by session id) for sole-session fallback.
    by_session_id: HashMap<String, ActiveSession>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, RegistryInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Register (or re-bind) a root agent to an active session.
    pub fn register(&self, agent_id: Uuid, active: ActiveSession) {
        let sid = active.id();
        let mut g = self.lock();
        g.by_agent.insert(agent_id, active.clone());
        g.by_session_id.insert(sid, active);
    }

    /// Drop a root agent binding. Removes the session from the sole-session set
    /// only when no other agent still points at the same session id.
    pub fn unregister_agent(&self, agent_id: Uuid) {
        let mut g = self.lock();
        let Some(active) = g.by_agent.remove(&agent_id) else {
            return;
        };
        let sid = active.id();
        let still_referenced = g.by_agent.values().any(|s| s.id() == sid);
        if !still_referenced {
            g.by_session_id.remove(&sid);
        }
    }

    /// Number of distinct registered sessions.
    pub fn session_count(&self) -> usize {
        self.lock().by_session_id.len()
    }
}

impl ActiveSessionResolver for SessionRegistry {
    fn resolve(&self, agent_id: Uuid) -> Option<ActiveSession> {
        let g = self.lock();
        if let Some(active) = g.by_agent.get(&agent_id) {
            return Some(active.clone());
        }
        // Sole-session fallback: CLI (one session) + unregistered subagent ids.
        if g.by_session_id.len() == 1 {
            return g.by_session_id.values().next().cloned();
        }
        None
    }
}

// ---------------------------------------------------------------------------
// Live session + Repl
// ---------------------------------------------------------------------------

/// One open session: agent, metadata handle, cancel token, model id.
///
/// Presentation-layer event buses (CLI sink, server broadcast) are injected via
/// the [`EventSink`] passed at construction — not owned here.
pub struct LiveSession {
    /// Serialized turns: only one `interact` runs at a time per session.
    pub agent: AsyncMutex<Agent>,
    /// Shared session document (title/scratchpad/links + persisted messages).
    pub active: ActiveSession,
    /// Cancel handle for the turn currently in flight (if any).
    pub cancel: Mutex<Option<CancelToken>>,
    /// Model id backing this session's agent.
    pub model: Model,
}

/// Shared REPL core: harness + config + multi-session registry.
#[derive(Clone)]
pub struct Repl {
    inner: Arc<ReplInner>,
}

struct ReplInner {
    harness: Arc<Harness>,
    registry: Arc<SessionRegistry>,
    /// Open sessions keyed by session id.
    live: Mutex<HashMap<String, Arc<LiveSession>>>,
    default_effort: Effort,
    debug_dump_api_requests: bool,
}

impl Repl {
    /// Attach a harness with registry-backed `session_meta` + `session_history`
    /// root services, and return the REPL core.
    pub async fn attach(
        config: HarnessConfig,
        default_effort: Effort,
        debug_dump_api_requests: bool,
    ) -> Result<Self, String> {
        let registry = Arc::new(SessionRegistry::new());
        let session_tool = Arc::new(SessionMetaTool::with_resolver(
            registry.clone() as Arc<dyn ActiveSessionResolver>
        )) as Arc<dyn ToolService>;
        let history_tool = Arc::new(SessionHistoryTool::new()) as Arc<dyn ToolService>;
        let harness =
            Harness::attach_with_root_services(config, vec![session_tool, history_tool]).await?;
        Ok(Self {
            inner: Arc::new(ReplInner {
                harness,
                registry,
                live: Mutex::new(HashMap::new()),
                default_effort,
                debug_dump_api_requests,
            }),
        })
    }

    pub fn harness(&self) -> &Arc<Harness> {
        &self.inner.harness
    }

    pub fn registry(&self) -> &Arc<SessionRegistry> {
        &self.inner.registry
    }

    pub fn default_effort(&self) -> Effort {
        self.inner.default_effort
    }

    pub fn debug_dump_api_requests(&self) -> bool {
        self.inner.debug_dump_api_requests
    }

    /// Build a model using this REPL's harness tools and system prompt.
    pub fn build_model(
        &self,
        model_id: Model,
        effort: Effort,
    ) -> Result<Arc<dyn GenerativeModel>, String> {
        build_model(
            model_id,
            &self.inner.harness,
            self.inner.debug_dump_api_requests,
            effort,
        )
    }

    /// Currently open (in-memory) sessions as `(id, model, title)` snapshots.
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

    /// Create a brand-new session and register it as live.
    pub fn create_session(
        &self,
        model: Model,
        effort: Option<Effort>,
        sink: Arc<dyn EventSink>,
    ) -> Result<Arc<LiveSession>, String> {
        let session = Session::new(model);
        self.register_live(
            session,
            model,
            effort.unwrap_or(self.inner.default_effort),
            sink,
        )
    }

    /// Open a saved session by id/prefix (or return it if already live).
    pub fn open_session(
        &self,
        id_or_prefix: &str,
        sink: Arc<dyn EventSink>,
    ) -> Result<Arc<LiveSession>, String> {
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
        self.register_live(session, model, self.inner.default_effort, sink)
    }

    /// After [`ActiveSession::replace`] changes the session id (CLI `/new`,
    /// `/resume`, `/compact`), re-key the live map so lookups stay correct.
    pub fn rekey_live_session(&self, old_id: &str, live: &Arc<LiveSession>) {
        let new_id = live.active.id();
        if old_id == new_id {
            return;
        }
        let mut g = self.lock_live();
        g.remove(old_id);
        g.insert(new_id.clone(), live.clone());
        // Keep registry sole-session set in sync with the new id.
        // Agent binding still points at the same ActiveSession Arc.
        drop(g);
        let mut reg = self.inner.registry.lock();
        reg.by_session_id.remove(old_id);
        reg.by_session_id.insert(new_id, live.active.clone());
    }

    /// Drop a live session (and its agent → session binding).
    pub fn close_session(&self, id: &str) {
        let removed = self.lock_live().remove(id);
        if let Some(live) = removed {
            // Best-effort: find agent_id(s) bound to this ActiveSession.
            let sid = live.active.id();
            let mut reg = self.inner.registry.lock();
            reg.by_agent.retain(|_, active| active.id() != sid);
            reg.by_session_id.remove(&sid);
        }
    }

    fn register_live(
        &self,
        session: Session,
        model: Model,
        effort: Effort,
        sink: Arc<dyn EventSink>,
    ) -> Result<Arc<LiveSession>, String> {
        let id = session.id.clone();
        let messages = session.messages.clone();
        let active = ActiveSession::new(session);

        let model_impl = self.build_model(model, effort)?;
        let mut agent = Agent::with_context(
            model_impl,
            self.inner.harness.clone(),
            sink,
            TraceContext::root(),
        );
        agent.set_context_window_tokens(model.context_window_tokens());
        agent.set_history(messages);
        let agent_id = agent.context().agent_id;

        let live = Arc::new(LiveSession {
            agent: AsyncMutex::new(agent),
            active: active.clone(),
            cancel: Mutex::new(None),
            model,
        });

        self.inner.registry.register(agent_id, active);
        self.lock_live().insert(id, live.clone());
        Ok(live)
    }

    fn lock_live(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<LiveSession>>> {
        self.inner.live.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::NullEventSink;
    use crate::tool_services::HostDispatchContext;

    #[test]
    fn registry_resolves_by_agent_and_sole_fallback() {
        let reg = SessionRegistry::new();
        let active = ActiveSession::new(Session::new(Model::ClaudeHaiku45));
        let root = Uuid::new_v4();
        reg.register(root, active.clone());

        assert_eq!(reg.resolve(root).unwrap().id(), active.id());
        // Unregistered (subagent) id → sole session.
        assert_eq!(reg.resolve(Uuid::new_v4()).unwrap().id(), active.id());
        assert_eq!(reg.session_count(), 1);
    }

    #[test]
    fn registry_no_fallback_with_multiple_sessions() {
        let reg = SessionRegistry::new();
        let a = ActiveSession::new(Session::new(Model::ClaudeHaiku45));
        let b = ActiveSession::new(Session::new(Model::ClaudeHaiku45));
        let id_a = Uuid::new_v4();
        let id_b = Uuid::new_v4();
        reg.register(id_a, a.clone());
        reg.register(id_b, b.clone());

        assert_eq!(reg.resolve(id_a).unwrap().id(), a.id());
        assert_eq!(reg.resolve(id_b).unwrap().id(), b.id());
        assert!(reg.resolve(Uuid::new_v4()).is_none());
    }

    #[tokio::test]
    async fn session_meta_via_registry_resolver() {
        let dir = std::env::temp_dir().join(format!(
            "myco-repl-meta-{}",
            crate::session::uuid_simple_hex(Uuid::new_v4())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("MYCO_HOME", &dir);
        }

        let reg = Arc::new(SessionRegistry::new());
        let active = ActiveSession::new(Session::new(Model::ClaudeHaiku45));
        let agent_id = Uuid::new_v4();
        reg.register(agent_id, active.clone());

        let tool = Arc::new(SessionMetaTool::with_resolver(
            reg.clone() as Arc<dyn ActiveSessionResolver>
        ));
        let result = tool
            .clone()
            .dispatch_tool_use(
                generative_model::ToolUse {
                    id: "t1".into(),
                    name: "session_meta".into(),
                    input: serde_json::json!({
                        "action": "set_title",
                        "title": "From Registry"
                    }),
                },
                HostDispatchContext::bare(agent_id, CancelToken::new()),
            )
            .await;
        assert!(!result.is_error, "{result:?}");
        assert_eq!(active.snapshot().title.as_deref(), Some("From Registry"));

        // Subagent id still resolves via sole-session fallback.
        let got = tool
            .dispatch_tool_use(
                generative_model::ToolUse {
                    id: "t2".into(),
                    name: "session_meta".into(),
                    input: serde_json::json!({"action": "get"}),
                },
                HostDispatchContext::bare(Uuid::new_v4(), CancelToken::new()),
            )
            .await;
        assert!(!got.is_error, "{got:?}");

        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::remove_var("MYCO_HOME");
        }
    }

    #[tokio::test]
    async fn create_session_registers_live_and_agent() {
        let dir = std::env::temp_dir().join(format!(
            "myco-repl-create-{}",
            crate::session::uuid_simple_hex(Uuid::new_v4())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        unsafe {
            std::env::set_var("MYCO_HOME", &dir);
        }

        let repl = Repl::attach(
            crate::harness::HarnessConfig {
                remote_hosts: vec![],
                enable_subagent: false,
                attach_timeout_secs: 5,
            },
            Effort::High,
            false,
        )
        .await
        .expect("attach");

        let sink = Arc::new(NullEventSink) as Arc<dyn EventSink>;
        let live = repl
            .create_session(Model::ClaudeHaiku45, None, sink)
            .expect("create");
        let agent_id = {
            let agent = live.agent.lock().await;
            agent.context().agent_id
        };
        assert!(repl.get_live(&live.active.id()).is_some());
        assert_eq!(
            repl.registry().resolve(agent_id).unwrap().id(),
            live.active.id()
        );

        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::remove_var("MYCO_HOME");
        }
    }
}

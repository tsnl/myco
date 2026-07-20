//! Transport-agnostic REPL core.
//!
//! Owns the shared [`Harness`], the resolved startup [`Config`] (model
//! catalog, host pool), a multi-session registry, and the single source of
//! truth for the system-prompt prologue and model construction. Front-ends
//! (CLI, future HTTP server) are thin adapters over this type: the general
//! case is multi-session; the CLI drives one live session at a time.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::Mutex as AsyncMutex;
use uuid::Uuid;

use crate::CancelToken;
use crate::config::Config;
use crate::generative_model::{
    self, BackendConfig, CatalogModel, Effort, GenerativeModel, GenerativeModelConfig,
};
use crate::harness::Harness;
use crate::prompts;
use crate::session::{ActiveSession, Agent, EventSink, Session, TraceContext};
use crate::tool_services::{
    ActiveSessionResolver, MemoryService, SessionHistoryTool, SessionMetaTool, ToolService,
};

/// System-prompt prologue shared by every root agent (CLI, server, …).
pub const SYSTEM_PROMPT_PROLOGUE: &str = r#"
You are a helpful assistant running in an agentic harness with unfettered computer access.
"#;

/// Build a generative model wired to `harness` tools and the shared system
/// prompt. The [`CatalogModel`] carries the backend (gateway + credentials)
/// resolved by [`Config`]; `effort` and the debug flag are applied on top.
pub fn build_model(
    catalog_model: &CatalogModel,
    harness: &Harness,
    debug_dump_api_requests: bool,
    effort: Effort,
) -> Result<Arc<dyn GenerativeModel>, String> {
    let mut backend_config = catalog_model.backend.clone();
    match &mut backend_config {
        BackendConfig::Anthropic(c) => {
            if debug_dump_api_requests {
                c.debug_dump_api_requests = true;
            }
            // Always enable thinking; effort controls how hard the model thinks.
            c.effort = Some(effort);
        }
        BackendConfig::OpenAIResponses(c) => {
            if debug_dump_api_requests {
                c.debug_dump_api_requests = true;
            }
            c.effort = Some(effort);
        }
    }

    generative_model::new(GenerativeModelConfig {
        model: catalog_model.spec.clone(),
        tools: harness.tool_specs(),
        system_prompt: [
            SYSTEM_PROMPT_PROLOGUE,
            prompts::DEFAULT_AGENT_PROMPT_EPILOGUE,
        ]
        .join("\n"),
        backend_config,
    })
    .map_err(|e| format!("failed to create model {}: {e}", catalog_model.spec.key))
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

/// One open session: agent, metadata handle, cancel token, catalog model.
///
/// Presentation-layer event buses (CLI sink, future server broadcast) are
/// injected via the [`EventSink`] passed at registration — not owned here.
pub struct LiveSession {
    /// Serialized turns: only one `interact` runs at a time per session.
    pub agent: AsyncMutex<Agent>,
    /// Shared session document (title/scratchpad/links + persisted messages).
    pub active: ActiveSession,
    /// Cancel handle for the turn currently in flight (if any).
    pub cancel: Mutex<Option<CancelToken>>,
    /// Catalog model backing this session's agent.
    pub model: CatalogModel,
}

/// Shared REPL core: harness + resolved config + multi-session registry.
#[derive(Clone)]
pub struct Repl {
    inner: Arc<ReplInner>,
}

struct ReplInner {
    harness: Arc<Harness>,
    registry: Arc<SessionRegistry>,
    /// Open sessions keyed by session id.
    live: Mutex<HashMap<String, Arc<LiveSession>>>,
    /// Resolved startup configuration (model catalog, host pool, defaults).
    config: Config,
    default_effort: Effort,
    debug_dump_api_requests: bool,
}

impl Repl {
    /// Attach a harness with registry-backed `session_meta`, `session_history`,
    /// and `memory` root services, and return the REPL core. `config` is the
    /// startup [`Config`] resolved once in `main`; the harness host pool and
    /// the model catalog come from it.
    pub async fn attach(
        config: Config,
        default_effort: Effort,
        debug_dump_api_requests: bool,
    ) -> Result<Self, String> {
        let registry = Arc::new(SessionRegistry::new());
        let session_tool = Arc::new(SessionMetaTool::with_resolver(
            registry.clone() as Arc<dyn ActiveSessionResolver>
        )) as Arc<dyn ToolService>;
        let history_tool = Arc::new(SessionHistoryTool::new()) as Arc<dyn ToolService>;
        let memory_tool = Arc::new(MemoryService::new()) as Arc<dyn ToolService>;
        let harness = Harness::attach_with_root_services(
            config.harness.clone(),
            vec![session_tool, history_tool, memory_tool],
        )
        .await?;
        Ok(Self {
            inner: Arc::new(ReplInner {
                harness,
                registry,
                live: Mutex::new(HashMap::new()),
                config,
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

    /// The resolved startup configuration this REPL was attached with.
    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    pub fn default_effort(&self) -> Effort {
        self.inner.default_effort
    }

    pub fn debug_dump_api_requests(&self) -> bool {
        self.inner.debug_dump_api_requests
    }

    /// Look up a catalog model by key; `None` → the configured default model.
    pub fn catalog_model(&self, key: Option<&str>) -> Result<CatalogModel, String> {
        let key = key.unwrap_or(&self.inner.config.model);
        self.inner.config.models.get(key).cloned()
    }

    /// Build a model using this REPL's harness tools and system prompt.
    pub fn build_model(
        &self,
        catalog_model: &CatalogModel,
        effort: Effort,
    ) -> Result<Arc<dyn GenerativeModel>, String> {
        build_model(
            catalog_model,
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

    /// Create a brand-new session on `model_key` (`None` → configured default)
    /// and register it as live.
    pub fn create_session(
        &self,
        model_key: Option<&str>,
        effort: Option<Effort>,
        sink: Arc<dyn EventSink>,
    ) -> Result<Arc<LiveSession>, String> {
        let catalog_model = self.catalog_model(model_key)?;
        let session = Session::new(catalog_model.spec.key.clone());
        self.register_session(
            ActiveSession::new(session),
            catalog_model,
            effort.unwrap_or(self.inner.default_effort),
            sink,
        )
    }

    /// Open a saved session by id/prefix (or return it if already live). The
    /// session's recorded model key is used when it is still in the catalog;
    /// otherwise the configured default model backs the agent (the session
    /// keeps recording the key it was created with).
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
        let catalog_model = self
            .catalog_model(Some(&session.model))
            .or_else(|_| self.catalog_model(None))?;
        self.register_session(
            ActiveSession::new(session),
            catalog_model,
            self.inner.default_effort,
            sink,
        )
    }

    /// Register a caller-built [`ActiveSession`] as live: build the agent on
    /// `catalog_model`, load its history, and wire the session registry. The
    /// CLI uses this directly so it can hold the [`ActiveSession`] (console
    /// mirror, readline history) before the agent exists.
    pub fn register_session(
        &self,
        active: ActiveSession,
        catalog_model: CatalogModel,
        effort: Effort,
        sink: Arc<dyn EventSink>,
    ) -> Result<Arc<LiveSession>, String> {
        let snapshot = active.snapshot();
        let id = snapshot.id;
        let messages = snapshot.messages;

        let model_impl = self.build_model(&catalog_model, effort)?;
        let mut agent = Agent::with_context(
            model_impl,
            self.inner.harness.clone(),
            sink,
            TraceContext::root(),
        );
        agent.set_context_window_tokens(catalog_model.spec.context_window_tokens);
        agent.set_history(messages);
        let agent_id = agent.context().agent_id;

        let live = Arc::new(LiveSession {
            agent: AsyncMutex::new(agent),
            active: active.clone(),
            cancel: Mutex::new(None),
            model: catalog_model,
        });

        self.inner.registry.register(agent_id, active);
        self.lock_live().insert(id, live.clone());
        Ok(live)
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

    fn lock_live(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<LiveSession>>> {
        self.inner.live.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ConfigUserSettings};
    use crate::harness::parse_file_config_str;
    use crate::session::NullEventSink;
    use crate::tool_services::HostDispatchContext;

    /// One-model catalog with a literal auth token; no remote hosts.
    const TEST_CONFIG_TOML: &str = r#"
model = "haiku"

[models.haiku]
protocol = "anthropic-messages"
base_url = "https://api.anthropic.com"
auth = "test-token"
api_id = "claude-haiku-4-5"
context_window = 200_000
"#;

    fn test_config() -> Config {
        let mut config = Config::resolve_with(
            ConfigUserSettings::default(),
            |_| None,
            false,
            |_| parse_file_config_str(TEST_CONFIG_TOML),
            || Ok(Vec::new()),
            |p| Err(format!("no auth files in tests: {}", p.display())),
        )
        .expect("resolve test config");
        config.harness.enable_subagent = false;
        config.harness.attach_timeout_secs = 5;
        config
    }

    #[test]
    fn registry_resolves_by_agent_and_sole_fallback() {
        let reg = SessionRegistry::new();
        let active = ActiveSession::new(Session::new("haiku".to_string()));
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
        let a = ActiveSession::new(Session::new("haiku".to_string()));
        let b = ActiveSession::new(Session::new("haiku".to_string()));
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
        let active = ActiveSession::new(Session::new("haiku".to_string()));
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

        let repl = Repl::attach(test_config(), Effort::High, false)
            .await
            .expect("attach");

        let sink = Arc::new(NullEventSink) as Arc<dyn EventSink>;
        let live = repl.create_session(None, None, sink).expect("create");
        assert_eq!(live.model.spec.key, "haiku");
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

//! Agent-side harness: always-on in-process local host + optional remotes.
//!
//! - **local**: tools run in-process via [`HostController::in_process`] (no subprocess).
//! - **remotes**: lazy `ssh … myco --mode host` over NDJSON.
//! - **Root-only services**: extra [`ToolService`]s (e.g. `session_meta`, `subagent`)
//!   are installed only on the local worker at attach time — still host tools,
//!   configured on root.

use std::collections::HashMap;
use std::sync::Arc;

use crate::core::{Async, CancelToken};
use crate::generative_model;
use crate::session::{EventSink, TraceContext};
use crate::tool_services::ToolService;

mod config;
pub use config::{
    AuthEntry, FileConfig, GatewayEntry, ModelEntry, default_ssh_config_path, example_config_toml,
    load_file_config, load_ssh_host_aliases, parse_file_config_str, ssh_config_host_aliases,
    ssh_spawn_command,
};

// HostController lives in `crate::host` (in-process local or remote subprocess).
pub use crate::host::{HostConfig, HostController};

mod subagent_service;
pub use subagent_service::{AgentRootHandles, SubagentService};

mod preflight;
pub use preflight::{
    ExecutableCheckReport, StartupPreflight, check_expected_executables, print_startup_preflight,
};

mod ssh;
pub use ssh::{
    SshAgentPreflightReport, ensure_remote_ssh_identities, ssh_destination_from_command,
};

/// Snapshot of one configured host.
#[derive(Debug, Clone)]
pub struct HostStatus {
    pub name: String,
    /// Display command (`in-process` for local, ssh argv for remotes).
    pub command: Vec<String>,
    /// Live worker connection is currently open (always true for in-process local).
    pub connected: bool,
    /// True when tools run inside the agent process (local).
    pub in_process: bool,
    pub tools: Vec<String>,
    /// Last connect failure (if any); cleared after a successful connect.
    pub error: Option<String>,
}

/// Agent-facing tool runtime: routes tools to hosts.
pub struct Harness {
    /// All hosts including always-present `"local"`.
    hosts: HashMap<String, Arc<HostController>>,
    /// Full command lines for display (`in-process` stored as a single-token vec).
    host_commands: HashMap<String, Vec<String>>,
    /// Always `"local"` — tools that omit `host` run here.
    default_host: String,
    /// Union of tool names known across hosts (standard + root-only extras on local).
    host_tool_names: std::collections::HashSet<String>,
    /// Tools installed only on the in-process local worker (not remotes).
    ///
    /// These always route to `local` and **must not** receive the injected routing
    /// `host` field — their own schemas may use `host` for other purposes
    /// (e.g. `session_meta` worktree links).
    root_only_tool_names: std::collections::HashSet<String>,
    /// Cached tool specs advertised to the model (host field injected for multi-host tools).
    tool_specs: Vec<generative_model::ToolSpec>,
    /// Local host's text-search engine. Attach never indexes; the owning
    /// process opts in via [`Self::auto_index_local`].
    local_search: crate::text_search::TextSearchEngine,
}

/// How to construct a harness.
#[derive(Debug, Clone)]
pub struct HarnessConfig {
    /// Remote hosts only. Local is always added in-process by [`Harness::attach`].
    pub remote_hosts: Vec<HostConfig>,
    /// When true (default), register [`SubagentService`] as a local tool.
    pub enable_subagent: bool,
    /// Per-remote connect timeout in seconds on first tool use (`0` disables it).
    pub attach_timeout_secs: u64,
    /// Resolved model catalog for subagents (`crate::config`). Empty (default)
    /// means the `subagent` tool rejects every model key.
    pub models: crate::generative_model::ModelCatalog,
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            remote_hosts: Vec::new(),
            enable_subagent: true,
            attach_timeout_secs: 10,
            models: crate::generative_model::ModelCatalog::default(),
        }
    }
}

/// Resolve `myco --mode host` argv (used by tests that still spawn a local subprocess).
pub fn default_local_host_command() -> Vec<String> {
    vec![
        myco_program(),
        "--mode".into(),
        "host".into(),
        "--name".into(),
        "local".into(),
    ]
}

pub(crate) fn myco_program() -> String {
    // cargo integration tests set this when the package builds the binary.
    if let Ok(path) = std::env::var("CARGO_BIN_EXE_myco")
        && std::path::Path::new(&path).is_file()
    {
        return path;
    }
    // Sibling of current exe (installed layout / `cargo run` target dir).
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        for name in ["myco", "myco.exe"] {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return candidate.to_string_lossy().into_owned();
            }
        }
        // Unit tests run as target/debug/deps/… — walk up to target/{debug,release}/.
        if let Some(parent) = dir.parent() {
            for name in ["myco", "myco.exe"] {
                let candidate = parent.join(name);
                if candidate.is_file() {
                    return candidate.to_string_lossy().into_owned();
                }
            }
        }
    }
    // Dev fallback via CARGO_MANIFEST_DIR.
    if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
        for profile in ["debug", "release"] {
            let candidate = std::path::Path::new(&manifest)
                .join("target")
                .join(profile)
                .join("myco");
            if candidate.is_file() {
                return candidate.to_string_lossy().into_owned();
            }
        }
    }
    "myco".into()
}

impl Harness {
    /// Register the always-on in-process local host, optional remotes, and local tools.
    ///
    /// Remotes start with `conn = None` and connect on first tool call (bounded by
    /// `attach_timeout_secs`). Local is ready immediately — no subprocess.
    /// Register the always-on in-process local host and optional remotes.
    ///
    /// When `enable_subagent` is true, installs [`SubagentService`] on the **local**
    /// worker only. Use [`Self::attach_with_root_services`] to add more root-only services
    /// (e.g. `session_meta`).
    pub async fn attach(config: HarnessConfig) -> Result<Arc<Self>, String> {
        Self::attach_with_root_services(config, Vec::new()).await
    }

    /// Like [`Self::attach`], plus extra [`ToolService`]s installed **only** on the
    /// in-process `local` worker (configuration-layer local-only tools).
    pub async fn attach_with_root_services(
        config: HarnessConfig,
        root_services: Vec<Arc<dyn ToolService>>,
    ) -> Result<Arc<Self>, String> {
        for h in &config.remote_hosts {
            if h.name == "local" {
                return Err(
                    "remote host name \"local\" is reserved; local is always in-process".into(),
                );
            }
            if h.name.trim().is_empty() {
                return Err("remote host with empty name".into());
            }
        }

        let mut hosts = HashMap::new();
        let mut host_commands = HashMap::new();
        let mut host_tool_names = std::collections::HashSet::new();
        let mut root_only_tool_names = std::collections::HashSet::new();
        let mut tool_specs = Vec::new();
        let mut seen_tools = HashMap::<String, ()>::new();

        // Standard catalog (every host, including remotes) — inject routing `host`.
        for spec in crate::host::HostWorker::standard_tool_specs() {
            host_tool_names.insert(spec.name.clone());
            if seen_tools.insert(spec.name.clone(), ()).is_none() {
                tool_specs.push(inject_host_field(spec));
            }
        }

        // Root-only extras (subagent, session_meta, …) — local only; keep their schemas.
        let mut root_extras: Vec<Arc<dyn ToolService>> = Vec::new();
        if config.enable_subagent {
            root_extras
                .push(Arc::new(SubagentService::new(config.models.clone())) as Arc<dyn ToolService>);
        }
        root_extras.extend(root_services);

        for service in &root_extras {
            for spec in service.tool_specs() {
                host_tool_names.insert(spec.name.clone());
                root_only_tool_names.insert(spec.name.clone());
                if seen_tools.insert(spec.name.clone(), ()).is_none() {
                    // Do **not** inject routing `host` — root tools may use `host` themselves.
                    tool_specs.push(spec);
                }
            }
        }

        // Keep the local engine handle on the harness so the owning process
        // can request auto-indexing after attach; attach itself never indexes.
        let local_search = crate::text_search::TextSearchEngine::new();
        let mut local_services: Vec<Arc<dyn ToolService>> =
            crate::host::HostWorker::services_with_search(
                crate::tool_services::TextSearchToolService::with_engine(local_search.clone()),
            );
        local_services.extend(root_extras);

        let local_worker = Arc::new(crate::host::HostWorker::new("local", local_services));
        let local = HostController::in_process("local", local_worker);
        host_commands.insert("local".into(), vec!["in-process".into()]);
        hosts.insert("local".into(), local);

        let connect_timeout = config.attach_timeout_secs;
        for host_cfg in config.remote_hosts {
            host_commands.insert(host_cfg.name.clone(), host_cfg.command.clone());
            let name = host_cfg.name.clone();
            hosts.insert(
                name,
                HostController::with_timeout(host_cfg, connect_timeout),
            );
        }

        Ok(Arc::new(Self {
            hosts,
            host_commands,
            default_host: "local".into(),
            host_tool_names,
            root_only_tool_names,
            tool_specs,
            local_search,
        }))
    }

    /// Owner opt-in: discover and index skills / AGENTS.md under `cwd` on the
    /// in-process local host (background crawl on the blocking pool).
    ///
    /// Called by the interactive CLI entrypoint. Tests that merely attach a
    /// harness never index anything.
    pub fn auto_index_local(&self, cwd: std::path::PathBuf) {
        self.local_search.auto_index_under(cwd);
    }

    /// Test helper: attach only the in-process local host, no subagent.
    pub async fn attach_local_for_tests() -> Result<Arc<Self>, String> {
        Self::attach(HarnessConfig {
            enable_subagent: false,
            ..HarnessConfig::default()
        })
        .await
    }

    /// In-process harness for unit tests: local host only, with the given services
    /// (plus the standard catalog).
    pub fn local_with_services(extra: Vec<Arc<dyn ToolService>>) -> Arc<Self> {
        let local_search = crate::text_search::TextSearchEngine::new();
        let standard = crate::host::HostWorker::services_with_search(
            crate::tool_services::TextSearchToolService::with_engine(local_search.clone()),
        );
        let mut host_tool_names = std::collections::HashSet::new();
        let mut root_only_tool_names = std::collections::HashSet::new();
        let mut tool_specs = Vec::new();
        let mut seen = HashMap::<String, ()>::new();
        for service in &standard {
            for spec in service.tool_specs() {
                host_tool_names.insert(spec.name.clone());
                if seen.insert(spec.name.clone(), ()).is_none() {
                    tool_specs.push(inject_host_field(spec));
                }
            }
        }
        for service in &extra {
            for spec in service.tool_specs() {
                host_tool_names.insert(spec.name.clone());
                root_only_tool_names.insert(spec.name.clone());
                if seen.insert(spec.name.clone(), ()).is_none() {
                    tool_specs.push(spec);
                }
            }
        }
        let mut services = standard;
        services.extend(extra);
        let worker = Arc::new(crate::host::HostWorker::new("local", services));
        let local = HostController::in_process("local", worker);
        let mut hosts = HashMap::new();
        let mut host_commands = HashMap::new();
        hosts.insert("local".into(), local);
        host_commands.insert("local".into(), vec!["in-process".into()]);
        Arc::new(Self {
            hosts,
            host_commands,
            default_host: "local".into(),
            host_tool_names,
            root_only_tool_names,
            tool_specs,
            local_search,
        })
    }

    pub fn tool_specs(&self) -> Vec<generative_model::ToolSpec> {
        self.tool_specs.clone()
    }

    /// Always `"local"`.
    pub fn default_host(&self) -> &str {
        &self.default_host
    }

    pub fn host_names(&self) -> Vec<String> {
        let mut names: Vec<_> = self.hosts.keys().cloned().collect();
        names.sort();
        // Keep local first for display.
        if let Some(i) = names.iter().position(|n| n == "local") {
            let local = names.remove(i);
            names.insert(0, local);
        }
        names
    }

    /// Status table for configured hosts (local always ok/in-process; remotes idle/ok/DOWN).
    pub fn host_status(&self) -> Vec<HostStatus> {
        self.host_names()
            .into_iter()
            .map(|name| {
                let command = self.host_commands.get(&name).cloned().unwrap_or_default();
                let client = self.hosts.get(&name).expect("host map key");
                HostStatus {
                    connected: client.is_connected(),
                    in_process: client.is_in_process(),
                    tools: client.tool_specs().iter().map(|t| t.name.clone()).collect(),
                    error: client.last_error(),
                    name,
                    command,
                }
            })
            .collect()
    }

    pub fn dispatch_tool_use(
        self: Arc<Self>,
        mut tool_use: generative_model::ToolUse,
        sink: Arc<dyn EventSink>,
        context: TraceContext,
        cancel: CancelToken,
    ) -> Async<generative_model::ToolResult> {
        Box::pin(async move {
            let id = tool_use.id.clone();
            let name = tool_use.name.clone();

            if !self.host_tool_names.contains(&name) {
                return generative_model::ToolResult::err(format!("unknown tool '{name}'"))
                    .with_id(id);
            }

            // Root-only tools always run on the in-process local worker. Their schemas
            // may use `host` for non-routing purposes (e.g. session_meta worktree links),
            // so do not resolve/strip a routing host field for them.
            let host_name = if self.root_only_tool_names.contains(&name) {
                self.default_host.clone()
            } else {
                match resolve_host_for_call(&tool_use, &self.default_host) {
                    Ok(h) => h,
                    Err(e) => return generative_model::ToolResult::err(e).with_id(id),
                }
            };

            if !self.root_only_tool_names.contains(&name) {
                // Strip routing-only `host` before forwarding multi-host tools.
                strip_host_field(&mut tool_use);
            }

            let Some(client) = self.hosts.get(&host_name).cloned() else {
                let known = self.host_names().join(", ");
                return generative_model::ToolResult::err(format!(
                    "unknown host {host_name:?} (known: [{known}]; default={})",
                    self.default_host
                ))
                .with_id(id);
            };

            // Root handles for in-process local (subagent etc.); remotes ignore.
            let agent_root: Option<Arc<dyn std::any::Any + Send + Sync>> = if client.is_in_process()
            {
                Some(Arc::new(crate::harness::AgentRootHandles {
                    harness: self.clone(),
                    sink,
                    context: context.clone(),
                }) as Arc<dyn std::any::Any + Send + Sync>)
            } else {
                None
            };

            client
                .call_with_root(context.agent_id, tool_use, cancel, agent_root)
                .await
        })
    }

    /// Notify all hosts that `agent_id`'s session ended.
    ///
    /// Safe to call from [`Drop`]: schedules work on the current tokio runtime when
    /// available. Host process exit (via HostController drop) is the hard guarantee
    /// for remotes; in-process local reaps via the worker directly.
    pub fn notify_agent_finished(&self, agent_id: uuid::Uuid) {
        let clients: Vec<_> = self.hosts.values().cloned().collect();
        if clients.is_empty() {
            return;
        }
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                for c in clients {
                    if let Err(e) = c.agent_finished(agent_id).await {
                        eprintln!("warning: agent_finished on host {:?}: {e}", c.name);
                    }
                }
            });
        }
    }
}

/// Read optional `host` from tool input; default to `default_host` (always `"local"`).
fn resolve_host_for_call(
    tool_use: &generative_model::ToolUse,
    default_host: &str,
) -> Result<String, String> {
    match tool_use.input.get("host") {
        None | Some(serde_json::Value::Null) => Ok(default_host.to_string()),
        Some(serde_json::Value::String(s)) => {
            let s = s.trim();
            if s.is_empty() {
                Ok(default_host.to_string())
            } else {
                Ok(s.to_string())
            }
        }
        Some(other) => Err(format!(
            "tool input field `host` must be a string, got {other}"
        )),
    }
}

fn strip_host_field(tool_use: &mut generative_model::ToolUse) {
    if let serde_json::Value::Object(map) = &mut tool_use.input {
        map.remove("host");
    }
}

/// Inject optional `host` into a host tool's JSON schema so models can target machines.
fn inject_host_field(mut spec: generative_model::ToolSpec) -> generative_model::ToolSpec {
    let schema = &mut spec.input_schema;
    if !schema.is_object() {
        return spec;
    }
    let Some(props) = schema
        .as_object_mut()
        .and_then(|o| o.get_mut("properties"))
        .and_then(|p| p.as_object_mut())
    else {
        // Ensure properties object exists.
        if let Some(obj) = schema.as_object_mut() {
            obj.entry("properties")
                .or_insert_with(|| serde_json::json!({}));
            if let Some(props) = obj.get_mut("properties").and_then(|p| p.as_object_mut()) {
                props.insert(
                    "host".into(),
                    serde_json::json!({
                        "type": ["string", "null"],
                        "description":
                            "Target host name (optional; defaults to \"local\"). Local is always in-process; remotes are named in config. Sessions are per-host.",
                    }),
                );
            }
        }
        return spec;
    };

    if !props.contains_key("host") {
        props.insert(
            "host".into(),
            serde_json::json!({
                "type": ["string", "null"],
                "description":
                    "Target host name (optional; defaults to \"local\"). Local is always in-process; remotes are named in config. Sessions are per-host.",
            }),
        );
    }
    // Do not add `host` to required[].
    spec
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generative_model::ToolUse;
    use serde_json::json;

    #[test]
    fn resolve_host_defaults_and_overrides() {
        let tu = ToolUse {
            id: "1".into(),
            name: "bash".into(),
            input: json!({"command": "echo"}),
        };
        assert_eq!(resolve_host_for_call(&tu, "local").unwrap(), "local");

        let tu = ToolUse {
            id: "1".into(),
            name: "bash".into(),
            input: json!({"command": "echo", "host": "devbox"}),
        };
        assert_eq!(resolve_host_for_call(&tu, "local").unwrap(), "devbox");

        let tu = ToolUse {
            id: "1".into(),
            name: "bash".into(),
            input: json!({"host": "  "}),
        };
        assert_eq!(resolve_host_for_call(&tu, "local").unwrap(), "local");
    }

    #[test]
    fn strip_host_removes_only_routing_field() {
        let mut tu = ToolUse {
            id: "1".into(),
            name: "bash".into(),
            input: json!({"command": "echo", "host": "devbox", "timeout_ms": 500}),
        };
        strip_host_field(&mut tu);
        assert_eq!(tu.input, json!({"command": "echo", "timeout_ms": 500}));
    }

    #[test]
    fn inject_host_field_adds_property() {
        let spec = generative_model::ToolSpec {
            name: "bash".into(),
            description: "x".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" }
                }
            }),
        };
        let injected = inject_host_field(spec);
        assert!(
            injected.input_schema["properties"]["host"].is_object(),
            "{:?}",
            injected.input_schema
        );
        assert!(injected.input_schema["properties"]["command"].is_object());
    }

    #[test]
    fn standard_catalog_excludes_web_tools() {
        let names: Vec<_> = crate::host::HostWorker::standard_tool_specs()
            .into_iter()
            .map(|s| s.name)
            .collect();
        assert!(
            !names.iter().any(|n| n.starts_with("web_")),
            "remotes must not advertise web_* tools: {names:?}"
        );
        assert!(names.contains(&"bash".to_string()));
        assert!(names.contains(&"manual".to_string()));
        assert!(names.contains(&"lynx_tui_browser".to_string()));
    }

    #[test]
    fn lynx_tui_browser_is_standard_host_tool_with_routing_host() {
        let harness = Harness::local_with_services(Vec::new());
        let browser = harness
            .tool_specs()
            .into_iter()
            .find(|s| s.name == "lynx_tui_browser")
            .expect("lynx_tui_browser in standard catalog");
        let host = &browser.input_schema["properties"]["host"];
        assert!(
            host.is_object(),
            "lynx_tui_browser should get injected routing host: {host:?}"
        );
        let desc = host["description"].as_str().unwrap_or("");
        assert!(
            desc.contains("defaults to \"local\""),
            "expected routing host description, got {desc:?}"
        );
    }

    #[tokio::test]
    async fn root_only_tools_keep_host_field_and_run_local() {
        use crate::CancelToken;
        use crate::generative_model::Content;
        use crate::session::{ActiveSession, Session};
        use crate::tool_services::SessionMetaTool;

        let _guard = crate::session::lock_myco_home_for_test();
        let dir = std::env::temp_dir().join(format!(
            "myco-root-only-host-{}",
            crate::session::uuid_simple_hex(uuid::Uuid::new_v4())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: test-only env override; held under the myco-home lock.
        unsafe {
            std::env::set_var("MYCO_HOME", &dir);
        }

        let active = ActiveSession::new(Session::new("claude-haiku-4-5"));
        let meta = Arc::new(SessionMetaTool::new(active.clone())) as Arc<dyn ToolService>;
        let harness = Harness::local_with_services(vec![meta]);

        // Schema must keep session_meta's own `host` (worktree links), not dual-purpose routing.
        let meta_spec = harness
            .tool_specs()
            .into_iter()
            .find(|s| s.name == "session_meta")
            .expect("session_meta advertised");
        let host_desc = meta_spec.input_schema["properties"]["host"]["description"]
            .as_str()
            .unwrap_or("");
        assert!(
            host_desc.contains("worktree") || host_desc.contains("Host name"),
            "session_meta host should describe worktree links, got {host_desc:?}"
        );
        // Routing-only description is only for multi-host tools.
        assert!(
            !host_desc.contains("defaults to \"local\""),
            "routing host description should not overwrite session_meta: {host_desc}"
        );

        let result = harness
            .clone()
            .dispatch_tool_use(
                ToolUse {
                    id: "t1".into(),
                    name: "session_meta".into(),
                    input: json!({
                        "action": "add_link",
                        "link_kind": "worktree",
                        "host": "devbox",
                        "path": "/tmp/wt",
                        "branch": "feat/x"
                    }),
                },
                Arc::new(crate::session::NullEventSink),
                TraceContext::default(),
                CancelToken::new(),
            )
            .await;
        assert!(!result.is_error, "{result:?}");
        let text: String = result
            .content
            .iter()
            .filter_map(|c| match c {
                Content::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(text.contains("devbox"), "{text}");
        assert!(text.contains("/tmp/wt"), "{text}");
        let links = active.snapshot().links;
        assert_eq!(links.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::remove_var("MYCO_HOME");
        }
    }

    #[tokio::test]
    async fn local_is_always_present_and_connected() {
        let harness = Harness::attach(HarnessConfig {
            enable_subagent: false,
            ..HarnessConfig::default()
        })
        .await
        .expect("attach");
        assert_eq!(harness.default_host(), "local");
        let status = harness.host_status();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].name, "local");
        assert!(status[0].connected);
        assert!(status[0].in_process);
        assert_eq!(status[0].command, vec!["in-process".to_string()]);

        let r = harness
            .clone()
            .dispatch_tool_use(
                ToolUse {
                    id: "t".into(),
                    name: "bash".into(),
                    input: json!({"command": "printf 'on-local\\n'"}),
                },
                Arc::new(crate::NullEventSink),
                TraceContext::default(),
                CancelToken::new(),
            )
            .await;
        assert!(!r.is_error, "{r:?}");
        let text = tool_text(&r);
        assert!(text.contains("on-local"), "{text}");
    }

    /// Attaching a harness must not index anything: auto-indexing is an
    /// explicit owner request, so tests never pay discovery/embedding work.
    #[tokio::test]
    async fn attach_does_not_auto_index() {
        let harness = Harness::attach(HarnessConfig {
            enable_subagent: false,
            ..HarnessConfig::default()
        })
        .await
        .expect("attach");

        // Give any (buggy) background indexing a chance to register roots.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let r = harness
            .clone()
            .dispatch_tool_use(
                ToolUse {
                    id: "t".into(),
                    name: "indexed_exact_text_search".into(),
                    input: json!({"query": "anything"}),
                },
                Arc::new(crate::NullEventSink),
                TraceContext::default(),
                CancelToken::new(),
            )
            .await;
        assert!(r.is_error, "search must fail with empty index: {r:?}");
        assert!(
            tool_text(&r).contains("no directories indexed"),
            "{}",
            tool_text(&r)
        );
    }

    /// The owner's explicit request is what makes discovery targets
    /// (AGENTS.md & co.) searchable.
    #[tokio::test]
    async fn auto_index_local_is_owner_opt_in() {
        let dir = std::env::temp_dir().join(format!("myco-owner-index-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("AGENTS.md"),
            "# guidance\nunique_owner_optin_token here\n",
        )
        .unwrap();

        let harness = Harness::attach(HarnessConfig {
            enable_subagent: false,
            ..HarnessConfig::default()
        })
        .await
        .expect("attach");
        harness.auto_index_local(dir.clone());

        // The crawl is a background task; poll until the root is searchable.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        let hit = loop {
            let r = harness
                .clone()
                .dispatch_tool_use(
                    ToolUse {
                        id: "t".into(),
                        name: "indexed_exact_text_search".into(),
                        input: json!({"query": "unique_owner_optin_token"}),
                    },
                    Arc::new(crate::NullEventSink),
                    TraceContext::default(),
                    CancelToken::new(),
                )
                .await;
            if !r.is_error && tool_text(&r).contains("AGENTS.md") {
                break r;
            }
            if std::time::Instant::now() > deadline {
                panic!("owner-requested auto-index never became searchable: {r:?}");
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        };
        assert!(
            tool_text(&hit).contains("unique_owner_optin_token"),
            "{hit:?}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn multi_host_attach_and_route_by_host_field() {
        // Two remotes as local subprocesses (not SSH) to exercise routing without network.
        let program = myco_program();
        // Unit tests don't set CARGO_BIN_EXE_*; need a built binary. Skip rather
        // than flake on connect timeout when the binary is missing/stale.
        if program == "myco" || !std::path::Path::new(&program).is_file() {
            eprintln!("skip multi_host: no myco binary at {program:?} (cargo build --bin myco)");
            return;
        }
        let cfg = HarnessConfig {
            enable_subagent: false,
            // Host hello can be slow under parallel suite load (MiniLM seed).
            attach_timeout_secs: 60,
            remote_hosts: vec![
                HostConfig {
                    name: "a".into(),
                    command: vec![
                        program.clone(),
                        "--mode".into(),
                        "host".into(),
                        "--name".into(),
                        "a".into(),
                    ],
                    ssh_destination: None,
                },
                HostConfig {
                    name: "b".into(),
                    command: vec![
                        program,
                        "--mode".into(),
                        "host".into(),
                        "--name".into(),
                        "b".into(),
                    ],
                    ssh_destination: None,
                },
            ],
            ..Default::default()
        };
        let harness = Harness::attach(cfg).await.expect("attach");
        assert_eq!(harness.default_host(), "local");
        let status = harness.host_status();
        assert_eq!(status.len(), 3); // local + a + b
        let local = status.iter().find(|s| s.name == "local").unwrap();
        assert!(local.connected && local.in_process);
        assert!(
            status
                .iter()
                .filter(|s| s.name != "local")
                .all(|s| !s.connected)
        );

        // Default → local (in-process).
        let r = harness
            .clone()
            .dispatch_tool_use(
                ToolUse {
                    id: "t1".into(),
                    name: "bash".into(),
                    input: json!({"command": "printf 'on-local\\n'"}),
                },
                Arc::new(crate::NullEventSink),
                TraceContext::default(),
                CancelToken::new(),
            )
            .await;
        assert!(!r.is_error, "{r:?}");
        assert!(tool_text(&r).contains("on-local"), "{}", tool_text(&r));

        // Explicit host b (subprocess).
        let r = harness
            .clone()
            .dispatch_tool_use(
                ToolUse {
                    id: "t2".into(),
                    name: "bash".into(),
                    input: json!({"command": "printf 'on-b\\n'", "host": "b"}),
                },
                Arc::new(crate::NullEventSink),
                TraceContext::default(),
                CancelToken::new(),
            )
            .await;
        assert!(!r.is_error, "{r:?}");
        assert!(tool_text(&r).contains("on-b"), "{}", tool_text(&r));

        // Unknown host.
        let r = harness
            .dispatch_tool_use(
                ToolUse {
                    id: "t3".into(),
                    name: "bash".into(),
                    input: json!({"command": "true", "host": "nope"}),
                },
                Arc::new(crate::NullEventSink),
                TraceContext::default(),
                CancelToken::new(),
            )
            .await;
        assert!(r.is_error);
        assert!(tool_text(&r).contains("unknown host"), "{}", tool_text(&r));
    }

    #[tokio::test]
    async fn lazy_connect_failure_on_first_use() {
        let cfg = HarnessConfig {
            enable_subagent: false,
            attach_timeout_secs: 10,
            remote_hosts: vec![HostConfig {
                name: "ghost".into(),
                command: vec!["/nonexistent/myco-please-fail".into()],
                ssh_destination: None,
            }],
            ..Default::default()
        };
        let harness = Harness::attach(cfg)
            .await
            .expect("attach is lazy and does not probe remotes");
        let status = harness.host_status();
        let ghost = status.iter().find(|s| s.name == "ghost").unwrap();
        assert!(!ghost.connected, "{ghost:?}");
        assert!(ghost.error.is_none(), "no error until first use: {ghost:?}");

        let r = harness
            .clone()
            .dispatch_tool_use(
                ToolUse {
                    id: "t".into(),
                    name: "bash".into(),
                    input: json!({"command": "true", "host": "ghost"}),
                },
                Arc::new(crate::NullEventSink),
                TraceContext::default(),
                CancelToken::new(),
            )
            .await;
        assert!(r.is_error);
        let text = tool_text(&r);
        assert!(
            text.contains("ghost") || text.contains("spawn") || text.contains("No such"),
            "{text}"
        );
        let status = harness.host_status();
        let ghost = status.iter().find(|s| s.name == "ghost").unwrap();
        assert!(!ghost.connected, "{ghost:?}");
        assert!(ghost.error.is_some(), "{ghost:?}");
        // Local still fine.
        let local = status.iter().find(|s| s.name == "local").unwrap();
        assert!(local.connected && local.in_process);
    }

    fn tool_text(r: &generative_model::ToolResult) -> String {
        r.content
            .iter()
            .filter_map(|c| match c {
                generative_model::Content::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

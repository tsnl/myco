//! Meta crate: re-exports the myco pillars under their v1 module names and
//! hosts the Rocket API server (`myco::server`).

pub use myco_agent as agent;
pub use myco_config as config;
pub use myco_core as core;
pub use myco_machines::harness;
pub use myco_machines::host;
pub use myco_machines::tool_services;
pub use myco_models as generative_model;
pub use myco_prompts as prompts;
pub use myco_session as session;

pub mod admin;
pub mod cli;
pub mod client;
pub mod server;
pub mod subagent;
pub mod tui;
pub mod web;

// Flat conveniences (v1 `myco::X` surface, minus the TUI).
pub use myco_agent::{
    Agent, AgentEvent, AgentInteractionError, CompactWorkerError, EventSink, NullEventSink,
    TraceContext, compact_subagent_prompt, run_compact_worker,
};
pub use myco_config::{Config, ConfigUserSettings, load_file_config};
pub use myco_core::{CancelToken, uuid_simple_hex};
pub use myco_machines::harness::{
    ExecutableCheckReport, Harness, HarnessConfig, HostConfig, HostController, HostStatus,
    SshAgentPreflightReport, StartupPreflight, default_ssh_config_path,
    ensure_remote_ssh_identities, load_ssh_host_aliases, ssh_config_host_aliases,
};
pub use myco_machines::host::HostWorker;
pub use myco_machines::tool_services::{
    HostDispatchContext, ListRecentService, SessionHistoryTool, SessionMetaTool, ToolService,
};
pub use myco_prompts::manual::Article as ManualArticle;
pub use myco_session::{
    ActiveSession, CompactOutcome, ConsoleLog, SESSION_FILE_VERSION, Session, SessionKind,
    SessionLink, SessionListEntry, compact_session, link_compact_pair, select_tail,
};

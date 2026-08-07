//! The myco crate: session runtime, agent, multi-host harness, provider
//! backends, config, auth, prompts — one library behind one binary. The only
//! separate crates are `myco-api` (wire types, shared with the wasm GUI) and
//! `myco-gui` (the Yew client).

pub mod agent;
pub mod auth;
pub mod config;
pub mod core;
pub mod machines;
pub mod models;
pub mod prompts;
pub mod session;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub mod cli;
pub mod client;
pub mod server;
pub mod subagent;
pub mod web;

// Historical module aliases (`myco::harness`, `myco::generative_model`, …).
pub use crate::machines::harness;
pub use crate::machines::host;
pub use crate::machines::tool_services;
pub use crate::models as generative_model;

// Flat conveniences (v1 `myco::X` surface).
pub use crate::agent::{
    Agent, AgentEvent, AgentInteractionError, CompactWorkerError, EventSink, NullEventSink,
    TraceContext, compact_subagent_prompt, run_compact_worker,
};
pub use crate::config::{Config, ConfigUserSettings, load_file_config};
pub use crate::core::{CancelToken, uuid_simple_hex};
pub use crate::machines::harness::{
    Harness, HarnessConfig, HostConfig, HostController, HostStatus, default_ssh_config_path,
    fatal_startup_check, load_ssh_host_aliases, prelude_warning, ssh_config_host_aliases,
};
pub use crate::machines::host::HostWorker;
pub use crate::machines::tool_services::{HostDispatchContext, SessionMetaTool, ToolService};
pub use crate::session::{
    ActiveSession, CompactOutcome, SESSION_FILE_VERSION, Session, SessionKind, SessionListEntry,
    compact_session, link_compact_pair, select_tail,
};

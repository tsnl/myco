pub mod agent;
pub mod config;
pub mod core;
pub mod external_command;
/// The model layer, extracted to its own crate ([`myco_llm_wire`]) — three
/// streaming wire protocols behind one message model. Aliased here so call
/// sites read as `generative_model::` regardless of the crate name.
pub use myco_llm_wire as generative_model;
pub mod harness;
pub mod host;
pub mod manual;
pub mod prompts;
pub mod session;
pub mod session_browser;
pub mod tool_services;
pub mod tui;

#[cfg(test)]
pub(crate) mod test_support;

pub use agent::{
    Agent, AgentEvent, AgentInteractionError, CompactWorkerError, EventSink, NullEventSink,
    TraceContext, compact_subagent_prompt, run_compact_worker,
};
pub use config::{ColorMode, Config, ConfigUserSettings, WrapMode, load_file_config};
pub use core::{CancelToken, uuid_simple_hex};
pub use harness::{
    ExecutableCheckReport, Harness, HarnessConfig, HostConfig, HostController, HostStatus,
    SshAgentPreflightReport, StartupPreflight, default_ssh_config_path,
    ensure_remote_ssh_identities, load_ssh_host_aliases, ssh_config_host_aliases,
};
pub use host::HostWorker;
pub use manual::Article as ManualArticle;
pub use session::{
    ActiveSession, CompactOutcome, ConsoleLog, SESSION_FILE_VERSION, Session, SessionKind,
    SessionLink, SessionListEntry, compact_session, link_compact_pair, select_tail,
};
pub use tool_services::{
    HostDispatchContext, ListRecentService, SessionHistoryTool, SessionMetaTool, ToolService,
};

pub use myco_agent as agent;
pub mod chat;
pub mod config;
pub mod core;
pub mod external_command;
pub use myco_model as generative_model;
pub mod harness;
pub mod host;
pub mod manual;
pub mod prelude;
pub mod prompts;
pub mod session;
pub mod session_browser;
pub mod session_runtime;
pub mod tool_services;
pub mod tui;

#[cfg(test)]
pub(crate) mod test_support;

pub use agent::{
    Agent, AgentEvent, AgentInteractionError, EventSink, NullEventSink, ToolExecutor, TraceContext,
};
pub use chat::{CompactWorkerError, compact_subagent_prompt, run_compact_worker};
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
    SessionLink, SessionListEntry, Thread, compact_thread, select_tail,
};
pub use session_runtime::SessionRuntime;
pub use tool_services::{
    HostDispatchContext, ListRecentService, PreludeTool, SessionHistoryTool, SessionMetaTool,
    ToolService,
};

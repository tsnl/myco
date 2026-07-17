pub mod core;
pub mod generative_model;
pub mod harness;
pub mod host;
pub mod tool_services;
pub mod text_search;
pub mod manual;
pub mod prompts;
pub mod session;

pub use core::CancelToken;
pub use harness::{
    default_config_path, default_local_host_command, ensure_remote_ssh_identities,
    example_config_toml, load_harness_config, print_preflight_report, Harness, HarnessConfig,
    HostController, HostConfig, HostStatus, RemoteHostConfig, SubagentService, AgentRootHandles,
    SshAgentPreflightReport,
};
pub use host::HostWorker;
pub use manual::Article as ManualArticle;
pub use tool_services::{
    BrowserService, HostDispatchContext, ManualService, SessionMetaTool, TextSearchToolService,
    ToolService,
};
pub use text_search::TextSearchEngine;
pub use session::{
    uuid_simple_hex, ActiveSession, Agent, AgentEvent, AgentInteractionError, EventSink,
    NullEventSink, Session, SessionLink, SessionListEntry, TraceContext, SESSION_FILE_VERSION,
};

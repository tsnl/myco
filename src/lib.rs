pub mod config;
pub mod core;
pub mod external_command;
pub mod generative_model;
pub mod harness;
pub mod host;
pub mod manual;
pub mod prompts;
pub mod session;
pub mod text_search;
pub mod tool_services;
pub mod tui;

pub use config::{ColorMode, Config, ConfigUserSettings, WrapMode};
pub use core::CancelToken;
pub use harness::{
    AgentRootHandles, ExecutableCheckReport, Harness, HarnessConfig, HostConfig, HostController,
    HostStatus, SshAgentPreflightReport, StartupPreflight, SubagentService,
    default_local_host_command, default_ssh_config_path, ensure_remote_ssh_identities,
    example_config_toml, load_file_config, load_ssh_host_aliases, print_startup_preflight,
    ssh_config_host_aliases,
};
pub use host::HostWorker;
pub use manual::Article as ManualArticle;
pub use session::{
    ActiveSession, Agent, AgentEvent, AgentInteractionError, CompactOptions, CompactOutcome,
    ConsoleLog, EventSink, NullEventSink, SESSION_FILE_VERSION, Session, SessionKind, SessionLink,
    SessionListEntry, TraceContext, compact_session, compact_subagent_prompt, link_compact_pair,
    select_tail, uuid_simple_hex,
};
pub use text_search::TextSearchEngine;
pub use tool_services::{
    BrowserService, HostDispatchContext, ManualService, SessionHistoryTool,
    SessionMetaTool, TextSearchToolService, ToolService,
};

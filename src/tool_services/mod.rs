//! Host tool services (bash, text editor, manual, …).
//!
//! Registered on a [`crate::host::HostWorker`]. The **standard** catalog is the same on
//! every host. The agent **root** (in-process `local`) may instantiate additional
//! services at configuration time (e.g. `session_meta`, `subagent`) — still
//! [`ToolService`], just not installed on remotes.

use std::sync::Arc;

use crate::core::{Async, CancelToken};
use crate::generative_model;

pub mod bash_service;
pub use bash_service::BashService;

pub mod text_editor_service;
pub use text_editor_service::TextEditorService;

pub mod manual_service;
pub use manual_service::ManualService;

pub mod meta_tool_service;
pub use meta_tool_service::SessionMetaTool;

pub mod session_history_service;
pub use session_history_service::SessionHistoryTool;

pub mod browser_service;
pub use browser_service::BrowserService;

pub mod text_search_tool_service;
pub use text_search_tool_service::TextSearchToolService;

/// Ambient context for host tool-service invocations.
#[derive(Clone)]
pub struct HostDispatchContext {
    /// Agent that owns this call (root or subagent); used for session ownership.
    pub agent_id: uuid::Uuid,
    /// Cancel signal for the in-flight call / agent turn.
    pub cancel: CancelToken,
    /// Set by the agent harness for **in-process** dispatches only.
    ///
    /// Tools like `subagent` downcast this to harness-provided root handles.
    /// Remote NDJSON workers always leave this `None`.
    pub agent_root: Option<Arc<dyn std::any::Any + Send + Sync>>,
}

impl HostDispatchContext {
    /// Context for remote / tests that need no agent-root hooks.
    pub fn bare(agent_id: uuid::Uuid, cancel: CancelToken) -> Self {
        Self {
            agent_id,
            cancel,
            agent_root: None,
        }
    }
}

/// Best-effort SIGKILL of a whole process group: `kill(2)` with `-pgid`.
///
/// Tool children are spawned with `.process_group(0)`, so the leader pid is
/// also the pgid; killing only the leader would leave grandchildren orphaned
/// under init. Direct syscall (no external `kill` binary), sync, and safe to
/// call from `Drop`. Errors (e.g. group already gone) are ignored.
pub(crate) fn kill_process_group(pid: Option<u32>) {
    let Some(pid) = pid else {
        return;
    };
    // SAFETY: kill(2) takes a pid and a signal number; no pointers involved.
    unsafe {
        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
    }
}

/// A placeable host tool capability.
pub trait ToolService: Send + Sync + 'static {
    fn tool_specs(&self) -> Vec<generative_model::ToolSpec>;

    fn dispatch_tool_use(
        self: Arc<Self>,
        tool_use: generative_model::ToolUse,
        ctx: HostDispatchContext,
    ) -> Async<generative_model::ToolResult>;

    /// Called when an agent session ends so services can drop agent-scoped state
    /// (e.g. bash sessions owned by that agent). Default: no-op.
    fn on_agent_finished(&self, _agent_id: uuid::Uuid) {}
}

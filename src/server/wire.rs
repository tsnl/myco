//! Wire DTOs for the HTTP/SSE API.
//!
//! Core runtime types ([`AgentEvent`], [`TraceContext`]) intentionally do **not**
//! derive `serde` — wire concerns stay out of the CLI/runtime path. This module
//! is the single translation layer between runtime events and the browser client.
//!
//! [`Content`], [`ToolUse`], [`ToolResult`], and [`TurnEndReason`] already derive
//! `serde` (used by the session store), so they are reused directly.

use serde::Serialize;
use uuid::Uuid;

use crate::generative_model::{Content, TurnEndReason};
use crate::session::{AgentEvent, TraceContext};

/// Correlation context on every streamed event (subagent nesting via `depth`).
#[derive(Debug, Clone, Serialize)]
pub struct WireContext {
    pub agent_id: Uuid,
    pub depth: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_tool_use_id: Option<String>,
}

impl From<&TraceContext> for WireContext {
    fn from(c: &TraceContext) -> Self {
        Self {
            agent_id: c.agent_id,
            depth: c.depth,
            parent_tool_use_id: c.parent_tool_use_id.clone(),
        }
    }
}

/// Serializable mirror of [`AgentEvent`] for the SSE stream.
///
/// Tagged by `type` so the client can switch on the variant. Field names match
/// the runtime enum; nested `context` carries agent/tool correlation.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireEvent {
    AgentStarted {
        agent_id: Uuid,
        model: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_agent_id: Option<Uuid>,
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_tool_use_id: Option<String>,
        depth: usize,
    },
    TextDelta {
        text: String,
        context: WireContext,
    },
    ThinkingDelta {
        text: String,
        context: WireContext,
    },
    ToolStarted {
        tool_use_id: String,
        name: String,
        input: serde_json::Value,
        context: WireContext,
    },
    ToolFinished {
        tool_use_id: String,
        is_error: bool,
        context: WireContext,
    },
    TurnFinished {
        reason: TurnEndReason,
        context: WireContext,
    },
    AgentFinished {
        #[serde(skip_serializing_if = "Option::is_none")]
        log_path: Option<String>,
        is_error: bool,
        context: WireContext,
    },
    /// Not an [`AgentEvent`]: emitted by the server when a submitted turn errors
    /// or is cancelled, so the client can clear its "running" state.
    TurnError {
        message: String,
        session_id: String,
    },
}

impl From<&AgentEvent> for WireEvent {
    fn from(ev: &AgentEvent) -> Self {
        match ev {
            AgentEvent::AgentStarted {
                agent_id,
                model,
                parent_agent_id,
                parent_tool_use_id,
                depth,
            } => WireEvent::AgentStarted {
                agent_id: *agent_id,
                model: model.clone(),
                parent_agent_id: *parent_agent_id,
                parent_tool_use_id: parent_tool_use_id.clone(),
                depth: *depth,
            },
            AgentEvent::TextDelta { text, context } => WireEvent::TextDelta {
                text: text.clone(),
                context: context.into(),
            },
            AgentEvent::ThinkingDelta { text, context } => WireEvent::ThinkingDelta {
                text: text.clone(),
                context: context.into(),
            },
            AgentEvent::ToolStarted { tool_use, context } => WireEvent::ToolStarted {
                tool_use_id: tool_use.id.clone(),
                name: tool_use.name.clone(),
                input: tool_use.input.clone(),
                context: context.into(),
            },
            AgentEvent::ToolFinished {
                tool_use_id,
                is_error,
                context,
            } => WireEvent::ToolFinished {
                tool_use_id: tool_use_id.clone(),
                is_error: *is_error,
                context: context.into(),
            },
            AgentEvent::TurnFinished { reason, context } => WireEvent::TurnFinished {
                reason: reason.clone(),
                context: context.into(),
            },
            AgentEvent::AgentFinished {
                log_path,
                is_error,
                context,
            } => WireEvent::AgentFinished {
                log_path: log_path.clone(),
                is_error: *is_error,
                context: context.into(),
            },
        }
    }
}

/// Answer/thinking content flattened for transcript rendering.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireContent {
    Text { text: String },
    Image { source: String },
    Thinking { text: String },
}

impl From<&Content> for WireContent {
    fn from(c: &Content) -> Self {
        match c {
            Content::Text { text } => WireContent::Text { text: text.clone() },
            Content::Image { source } => WireContent::Image {
                source: source.clone(),
            },
            Content::Thinking { text, .. } => WireContent::Thinking { text: text.clone() },
        }
    }
}

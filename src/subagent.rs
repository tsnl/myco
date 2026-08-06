//! The `subagent` tool: nested agents as a first-class tool call, no curl
//! required. Root-only — installed on the in-process local worker, never on
//! remotes — so nesting works on machines behind strict firewalls: the child
//! runs server-side and reaches remotes through the same host pool as the
//! parent.
//!
//! One call = one full child turn: create (or continue) a hidden child
//! session through the [`Server`], queue the prompt, wait on the child's
//! completed-turn counter, and return the child's final answer plus its
//! session id for follow-ups. The parent's cancel token cancels the child's
//! turn on the way down.

use std::sync::{Arc, Weak};

use crate::core::Async;
use crate::machines::tool_services::{HostDispatchContext, ToolService, tool_input_schema};
use crate::models::{self as generative_model};
use myco_api::{ToolResult, ToolUse};

use crate::server::{Server, last_answer};

const TOOL_DESCRIPTION: &str = r#"
Delegate to a nested agent: one call runs one full turn of a hidden child
session and returns its final answer.

Without `session_id`, a fresh child is created (`kind: subagent`, parented to
this session); `fork: true` seeds it with a copy of this conversation so far,
`model` picks a catalog model. The result starts with `session: <id>` — pass
that id back as `session_id` to continue the same child with follow-up turns.

Prompts must be self-contained; ask for terse summaries. Children persist in
the shared session store, and people can watch a child live in the GUI's work
rail — or take it over. While someone holds a child, your calls to it are
refused; try again after they hand it back (the handoff shows up in this
conversation).
"#;

#[derive(schemars::JsonSchema, serde::Deserialize)]
struct Input {
    /// One self-contained prompt for the child's next turn.
    prompt: String,
    /// Continue this existing child instead of creating a fresh one.
    #[serde(default)]
    session_id: Option<String>,
    /// Model key from the catalog (fresh children only; default: server default).
    #[serde(default)]
    model: Option<String>,
    /// Seed a fresh child with a copy of the parent conversation (context fork).
    #[serde(default)]
    fork: bool,
}

/// Root-only [`ToolService`] wired per parent session (knows its parent id).
/// Holds the [`Server`] weakly: the server owns the supervisor, tools
/// must not keep it alive.
pub struct SubagentTool {
    supervisor: Weak<Server>,
    parent_id: String,
}

impl SubagentTool {
    pub(crate) fn new(supervisor: Weak<Server>, parent_id: String) -> Self {
        Self {
            supervisor,
            parent_id,
        }
    }

    async fn run(&self, input: Input, ctx: HostDispatchContext) -> Result<String, String> {
        let sup = self
            .supervisor
            .upgrade()
            .ok_or_else(|| "server shutting down".to_string())?;

        let child = match input.session_id.as_deref().map(str::trim) {
            Some(id) if !id.is_empty() => {
                if self.parent_id.starts_with(id) {
                    return Err("subagent session_id refers to this session itself".into());
                }
                sup.ensure_live(id, None).await?
            }
            _ => {
                let fresh = sup.new_child(&self.parent_id, input.model.clone(), input.fork)?;
                let id = fresh.id.clone();
                sup.ensure_live(&id, Some(fresh)).await?
            }
        };
        let child_id = child.session.id();
        // The subagent keyboard, pointed away from us: a person took this
        // child over in the GUI. Refuse politely rather than interleave — the
        // same courtesy the bash tool extends on a user-held shell.
        if child.user_holds() {
            return Err(format!(
                "subagent session {child_id} is held by a person right now; \
                 try again once they hand it back"
            ));
        }

        let mut turns = child.turns.clone();
        let start = *turns.borrow();
        // The parent agent wrote this prompt; attribute it to the parent's
        // model rather than to a person.
        let parent_model = match sup.get_live(&self.parent_id).await {
            Some(parent) => parent.session.snapshot().model,
            None => child.session.snapshot().model,
        };
        child
            .tx
            .send(crate::server::Cmd::User {
                entry: sup.compose(
                    myco_api::Author::Agent {
                        model: parent_model,
                    },
                    &input.prompt,
                    &child.session.snapshot().model,
                ),
            })
            .map_err(|_| "child agent task gone".to_string())?;

        // Wait for the child's turn, staying cancellable: cancelling the
        // parent's turn cancels the child's on the way down.
        loop {
            tokio::select! {
                _ = ctx.cancel.cancelled() => {
                    child.cancel_turn().await;
                    return Err(format!("cancelled (child session: {child_id})"));
                }
                changed = turns.changed() => {
                    if changed.is_err() {
                        return Err(format!("child agent task gone (session: {child_id})"));
                    }
                    if *turns.borrow() > start {
                        break;
                    }
                }
            }
        }

        let snapshot = child.session.snapshot();
        let answer = last_answer(&snapshot.entries)
            .unwrap_or_else(|| "(child turn produced no answer text)".to_string());
        Ok(format!("session: {child_id}\n\n{answer}"))
    }
}

impl ToolService for SubagentTool {
    fn tool_specs(&self) -> Vec<generative_model::ToolSpec> {
        vec![generative_model::ToolSpec {
            name: "subagent".to_string(),
            description: TOOL_DESCRIPTION.to_string(),
            input_schema: tool_input_schema::<Input>(),
        }]
    }

    fn dispatch_tool_use(
        self: Arc<Self>,
        tool_use: ToolUse,
        ctx: HostDispatchContext,
    ) -> Async<ToolResult> {
        Box::pin(async move {
            let input: Input = match serde_json::from_value(tool_use.input.clone()) {
                Ok(v) => v,
                Err(e) => return ToolResult::err(format!("invalid subagent input: {e}")),
            };
            match self.run(input, ctx).await {
                Ok(text) => ToolResult::text(text),
                Err(e) => ToolResult::err(e),
            }
        })
    }
}

//! Root-only `subagent` tool service (installed on the in-process local worker).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::Harness;
use crate::core::Async;
use crate::generative_model::{self, Content, GenerativeModelConfig, Message, Model};
use crate::prompts;
use crate::session::{
    Agent, AgentEvent, EventSink, Session, SessionKind, TraceContext, uuid_simple_hex,
};
use crate::tool_services::{HostDispatchContext, ToolService};

const SUBAGENT_SYSTEM_PROMPT_PROLOGUE: &str = r#"
You are a subagent being instructed by a supervisor agent.

You will receive a single-shot instruction from the supervisor agent as a single user turn.
Respond with an appropriate response. Always include your subagent ID in the response to the
supervisor agent.

You share tools with the supervisor agent.
"#;

const TOOL_DESCRIPTION: &str = r#"
Runs a subagent with the specified prompt as input.

Single-shot, no multi-turn conversation, though the subagent may use multiple turns to process tool
calls before replying.

Creates a session under `~/.myco/session/` with `kind: subagent` (not visible in default listings)
whose id equals the subagent UUID (same as runtime agent_id). Accessible via get-by-id or
`session_meta list` with `include_hidden: true`.

The `model` field must be one of the supported model ids (see the tool input schema enum).
"#;

const SUBAGENT_LOG_DIR: &str = ".myco/subagent-logs";

/// Handles the agent harness injects into in-process [`HostDispatchContext::agent_root`].
pub struct AgentRootHandles {
    pub harness: Arc<Harness>,
    pub sink: Arc<dyn EventSink>,
    pub context: TraceContext,
}

/// Spawns a one-shot subagent that shares the supervisor's harness and event sink.
///
/// Only useful when registered on the root in-process local worker (needs
/// [`AgentRootHandles`]). Persists a **hidden** [`Session`] under
/// `~/.myco/session/` with `id == agent_id` (same hex as runtime correlation).
/// Also writes a debug transcript to `.myco/subagent-logs/{agent_id}.log`.
#[derive(Default)]
pub struct SubagentService {}

impl SubagentService {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ToolService for SubagentService {
    fn tool_specs(&self) -> Vec<generative_model::ToolSpec> {
        vec![generative_model::ToolSpec {
            name: "subagent".to_string(),
            description: TOOL_DESCRIPTION.to_string(),
            input_schema: schemars::schema_for!(Input).to_value(),
        }]
    }

    fn dispatch_tool_use(
        self: Arc<Self>,
        tool_use: generative_model::ToolUse,
        ctx: HostDispatchContext,
    ) -> Async<generative_model::ToolResult> {
        Box::pin(async move {
            let input: Input = match serde_json::from_value(tool_use.input.clone()) {
                Ok(input) => input,
                Err(e) => {
                    return generative_model::ToolResult::err(format!(
                        "Error deserializing subagent input: {e}"
                    ));
                }
            };
            self.execute(input, tool_use.id, ctx).await
        })
    }
}

impl SubagentService {
    async fn execute(
        &self,
        input: Input,
        parent_tool_use_id: String,
        ctx: HostDispatchContext,
    ) -> generative_model::ToolResult {
        let Some(root_any) = ctx.agent_root.as_ref() else {
            return generative_model::ToolResult::err(
                "subagent requires agent root handles (in-process local host only)",
            );
        };
        let Some(root) = root_any.downcast_ref::<AgentRootHandles>() else {
            return generative_model::ToolResult::err(
                "subagent: agent_root is not AgentRootHandles",
            );
        };

        let model = match generative_model::new(GenerativeModelConfig {
            model: input.model,
            tools: root.harness.tool_specs(),
            system_prompt: [
                SUBAGENT_SYSTEM_PROMPT_PROLOGUE,
                prompts::DEFAULT_AGENT_PROMPT_EPILOGUE,
            ]
            .join("\n\n"),
            backend_config: None,
        }) {
            Ok(m) => m,
            Err(e) => {
                return generative_model::ToolResult::err(format!(
                    "Failed to create subagent model: {e:?}"
                ));
            }
        };

        let agent_id = uuid::Uuid::new_v4();
        let agent_id_hex = uuid_simple_hex(agent_id);
        let log_path = PathBuf::from(SUBAGENT_LOG_DIR).join(format!("{agent_id_hex}.log"));

        let parent_session_id = uuid_simple_hex(root.context.agent_id);
        let mut worker_session = Session::new_hidden(
            input.model,
            agent_id_hex.clone(),
            SessionKind::Subagent,
            Some(parent_session_id.clone()),
        );
        worker_session.title = Some(format!("subagent of {parent_session_id}"));
        if let Err(e) = worker_session.save() {
            eprintln!(
                "warning: failed to create hidden subagent session {agent_id_hex}: {e}"
            );
        }

        let child_context = root
            .context
            .child_agent(agent_id, Some(parent_tool_use_id.clone()));

        root.sink.emit(AgentEvent::AgentStarted {
            agent_id,
            model: input.model.api_id().to_string(),
            parent_agent_id: Some(root.context.agent_id),
            parent_tool_use_id: Some(parent_tool_use_id.clone()),
            depth: child_context.depth,
        });

        let mut subagent = Agent::with_context(
            model,
            root.harness.clone(),
            root.sink.clone(),
            child_context.clone(),
        );

        let id_notice = format!(
            "Your subagent UUID is {agent_id_hex} (hidden session id). \
             Write durable details to `{}` if needed; the harness also persists \
             this session under ~/.myco/session/ and a debug log at that path.",
            log_path.display()
        );

        let interact_result = subagent
            .interact(
                vec![
                    Content::Text {
                        text: input.prompt.clone(),
                    },
                    Content::Text { text: id_notice },
                ],
                ctx.cancel.clone(),
            )
            .await;

        // Persist full history into the hidden session (source of truth).
        worker_session.messages = subagent.history().to_vec();
        worker_session.touch();
        if let Err(e) = worker_session.save() {
            eprintln!(
                "warning: failed to save hidden subagent session {agent_id_hex}: {e}"
            );
        }

        if let Err(e) = write_subagent_log(
            &log_path,
            agent_id,
            input.model.api_id(),
            Some(&parent_tool_use_id),
            &input.prompt,
            subagent.history(),
            interact_result.as_ref().ok().map(|v| v.as_slice()),
            interact_result.as_ref().err().map(|e| e.to_string()),
        ) {
            eprintln!(
                "warning: failed to write subagent log {}: {e}",
                log_path.display()
            );
        }

        match interact_result {
            Ok(content) => {
                let text = content
                    .iter()
                    .filter_map(|c| match c {
                        Content::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                generative_model::ToolResult::text(if text.is_empty() {
                    format!(
                        "(subagent {agent_id_hex} finished with no text; hidden session={agent_id_hex})"
                    )
                } else {
                    format!("{text}\n\n(subagent session={agent_id_hex})")
                })
            }
            Err(e) => generative_model::ToolResult::err(format!("subagent failed: {e}")),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn write_subagent_log(
    path: &Path,
    agent_id: uuid::Uuid,
    model: &str,
    parent_tool_use_id: Option<&str>,
    prompt: &str,
    history: &[Message],
    final_content: Option<&[Content]>,
    error: Option<String>,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let mut file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    writeln!(file, "agent_id={}", uuid_simple_hex(agent_id)).map_err(|e| e.to_string())?;
    writeln!(file, "model={model}").map_err(|e| e.to_string())?;
    if let Some(p) = parent_tool_use_id {
        writeln!(file, "parent_tool_use_id={p}").map_err(|e| e.to_string())?;
    }
    writeln!(file, "\n## prompt\n{prompt}\n").map_err(|e| e.to_string())?;
    writeln!(file, "## history ({} messages)", history.len()).map_err(|e| e.to_string())?;
    for (i, msg) in history.iter().enumerate() {
        writeln!(file, "\n### message {i}\n{msg:#?}").map_err(|e| e.to_string())?;
    }
    if let Some(content) = final_content {
        writeln!(file, "\n## final_content\n{content:#?}").map_err(|e| e.to_string())?;
    }
    if let Some(err) = error {
        writeln!(file, "\n## error\n{err}").map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[derive(
    Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize, PartialEq, Eq,
)]
struct Input {
    /// Instruction for the subagent (single user turn).
    prompt: String,
    /// Model to run for the subagent (see schema enum for supported ids).
    model: Model,
}

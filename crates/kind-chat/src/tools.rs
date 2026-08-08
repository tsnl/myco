//! The named-tool dispatcher: the model's tools become verbs on the bus,
//! called as the chat's own agent principal. The model never sees a
//! generic `call` — unification is for the substrate, never the prompt
//! (DESIGN.md) — and the bus never sees a privileged channel: the agent
//! creates, drives, and removes instances under exactly the rules any
//! principal lives by.
//!
//! One tool so far. `bash` runs a command as a one-shot tty instance:
//! create (the agent is creator and driver), wait on the watermark until
//! the child exits or the budget runs out, drain the scrollback, remove. The
//! terminal is a real pool instance while it lives — visible in the tree,
//! watchable by anyone, and cleaned up even when the turn is cancelled
//! mid-command (the removal rides a drop guard).

use std::time::Duration;

use myco_instance::{Pool, Principal, VerbError};

use crate::tail;
use myco_models::{ToolResult, ToolSpec, ToolUse};
use serde_json::{Value, json};

/// What the model may call. Handed to the provider at chat create; the
/// dispatcher below is the other half of the same contract.
pub(crate) fn tool_specs() -> Vec<ToolSpec> {
    vec![ToolSpec {
        name: "bash".into(),
        description: "Run a shell command on the workspace host. The command runs under \
                      `bash -c` in a fresh one-shot terminal; stdout and stderr come back \
                      interleaved, and the terminal is removed afterwards — state does not \
                      persist between calls, use files for that."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "the command line, run under `bash -c`"
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "kill the command after this many seconds \
                                    (default 120, max 600)"
                }
            },
            "required": ["command"]
        }),
    }]
}

/// Answer one tool call. Errors are results, never panics or wedges: the
/// model reads what went wrong and decides what to do about it.
pub(crate) async fn dispatch(
    pool: &Pool,
    agent: &Principal,
    project: &str,
    tool: &ToolUse,
) -> ToolResult {
    let outcome = match tool.name.as_str() {
        "bash" => bash(pool, agent, project, &tool.input).await,
        other => Err(format!("unknown tool {other:?} — available tools: bash")),
    };
    match outcome {
        Ok(text) => ToolResult::text(text).with_id(tool.id.clone()),
        Err(why) => ToolResult::err(why).with_id(tool.id.clone()),
    }
}

const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_TIMEOUT_SECS: u64 = 600;
/// Cap on what one command feeds back into context. The *end* of the
/// output survives truncation — exit chatter beats preamble.
const MAX_TOOL_OUTPUT_BYTES: usize = 48 * 1024;

async fn bash(
    pool: &Pool,
    agent: &Principal,
    project: &str,
    input: &Value,
) -> Result<String, String> {
    let command = input
        .get("command")
        .and_then(Value::as_str)
        .filter(|c| !c.trim().is_empty())
        .ok_or("bash needs a non-empty {command}")?;
    let timeout = input
        .get("timeout_secs")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .clamp(1, MAX_TIMEOUT_SECS);

    let title: String = format!("bash: {}", command.lines().next().unwrap_or(""))
        .chars()
        .take(48)
        .collect();
    let info = pool
        .create(agent, "tty", project, &title, json!({"command": command}))
        .map_err(|e| format!("cannot start a terminal: {e}"))?;
    // Removal is owed no matter how this function ends — normal return,
    // error, or the whole turn task aborted mid-await.
    let tty = RemoveOnDrop {
        pool: pool.clone(),
        agent: agent.clone(),
        id: info.id,
    };

    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout);
    let exited = pool
        .wait_until(agent, &tty.id, "text", deadline, |text| {
            text.get("running") != Some(&json!(true))
        })
        .await
        .map_err(|e| format!("terminal died: {e}"))?;

    let (mut output, truncated) = tail::drain(pool, agent, &tty.id, MAX_TOOL_OUTPUT_BYTES)
        .await
        .map_err(|e| format!("terminal died: {e}"))?;
    drop(tty);

    if truncated {
        output = format!("[output truncated to the last {MAX_TOOL_OUTPUT_BYTES} bytes]\n{output}");
    }
    if exited.is_none() {
        return Err(format!(
            "timed out after {timeout}s (the command was killed); output so far:\n{output}"
        ));
    }
    Ok(output)
}

/// Removes the tool's tty when dropped — including when the turn task is
/// aborted mid-command, which is exactly when nobody else would clean up.
struct RemoveOnDrop {
    pool: Pool,
    agent: Principal,
    id: String,
}

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let pool = self.pool.clone();
        let agent = self.agent.clone();
        let id = self.id.clone();
        tokio::spawn(async move {
            match pool.call(&agent, &id, "sys.remove", Value::Null).await {
                Ok(_) | Err(VerbError::UnknownInstance { .. }) | Err(VerbError::Gone) => {}
                Err(e) => eprintln!("myco: tool tty {id} not removed: {e}"),
            }
        });
    }
}

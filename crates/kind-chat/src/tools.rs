//! The named-tool dispatcher: the model's tools become verbs on the bus,
//! called as the chat's own agent principal. The model never sees a
//! generic `call` — unification is for the substrate, never the prompt
//! (DESIGN.md) — and the bus never sees a privileged channel: the agent
//! creates, drives, and removes instances under exactly the rules any
//! principal lives by.
//!
//! Two tools. `bash` runs a command as a one-shot tty instance: create
//! (the agent is creator and driver), watch the watermark until the child
//! exits or the budget runs out, drain the scrollback, remove. The
//! terminal is a real pool instance while it lives — visible in the tree,
//! watchable by anyone, and cleaned up even when the turn is cancelled
//! mid-command (the removal rides a drop guard).
//!
//! `subagent` is the ledger's "drop the child machinery" made concrete:
//! it creates an ordinary chat whose `parent` is this chat, posts the task
//! as this chat's agent principal (another agent's post starts the child's
//! turn — the trigger rule), waits for the child to settle, and splices
//! the answer back. The child chat *remains* in the pool afterwards —
//! subagents are reviewable work, not hidden plumbing. Depth is capped at
//! two levels of parentage; the refusal is a result the model reads.

use std::time::Duration;

use myco_instance::{Pool, Principal, VerbError};
use myco_models::{ToolResult, ToolSpec, ToolUse};
use serde_json::{Value, json};

/// What the model may call. Handed to the provider at chat create; the
/// dispatcher below is the other half of the same contract.
pub(crate) fn tool_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "subagent".into(),
            description: "Delegate a task to a subagent: a fresh chat, parented under this \
                          one, whose model works the task with the same tools and answers \
                          once. The reply comes back as this tool's result; the subagent \
                          chat stays in the workspace for review. Use it for work that is \
                          self-contained and would clutter this conversation."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task": {
                        "type": "string",
                        "description": "the full task statement, self-contained — the \
                                        subagent sees nothing of this conversation"
                    },
                    "model": {
                        "type": "string",
                        "description": "catalog key for the subagent's model \
                                        (default: this chat's model)"
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "give up after this many seconds \
                                        (default 600, max 3600)"
                    }
                },
                "required": ["task"]
            }),
        },
        ToolSpec {
        name: "bash".into(),
        description: "Run a shell command on the workspace host. The command runs under \
                      `bash -c` in a fresh one-shot terminal; stdout and stderr come back \
                      interleaved, a non-zero exit code makes the result an error, and the \
                      terminal is removed afterwards — state does not persist between \
                      calls, use files for that."
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
        },
    ]
}

/// Answer one tool call. Errors are results, never panics or wedges: the
/// model reads what went wrong and decides what to do about it.
pub(crate) async fn dispatch(
    pool: &Pool,
    agent: &Principal,
    project: &str,
    model: &str,
    tool: &ToolUse,
) -> ToolResult {
    let outcome = match tool.name.as_str() {
        "bash" => bash(pool, agent, project, &tool.input).await,
        "subagent" => subagent(pool, agent, project, model, &tool.input).await,
        other => Err(format!(
            "unknown tool {other:?} — available tools: bash, subagent"
        )),
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
        .create(
            agent,
            "tty",
            project,
            &title,
            json!({"command": command, "mode": "piped"}),
        )
        .map_err(|e| format!("cannot start a terminal: {e}"))?;
    // Removal is owed no matter how this function ends — normal return,
    // error, or the whole turn task aborted mid-await.
    let tty = RemoveOnDrop {
        pool: pool.clone(),
        agent: agent.clone(),
        id: info.id,
    };

    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout);
    let mut mark = 0;
    let mut exit: (Option<i64>, Option<i64>) = (None, None);
    let timed_out = loop {
        let text = pool
            .call(agent, &tty.id, "text", Value::Null)
            .await
            .map_err(|e| format!("terminal died: {e}"))?;
        if text.get("running") != Some(&json!(true)) {
            exit = (
                text.get("exit_code").and_then(Value::as_i64),
                text.get("exit_signal").and_then(Value::as_i64),
            );
            break false;
        }
        match tokio::time::timeout_at(deadline, pool.changed(&tty.id, mark)).await {
            Ok(Ok(m)) => mark = m,
            Ok(Err(e)) => return Err(format!("terminal died: {e}")),
            Err(_) => break true,
        }
    };

    // Drain the scrollback, keeping only the freshest budget's worth.
    let mut output = String::new();
    let mut truncated = false;
    let mut from = 0;
    loop {
        let page = pool
            .call(
                agent,
                &tty.id,
                "tail",
                json!({"from": from, "max_bytes": 65536}),
            )
            .await
            .map_err(|e| format!("terminal died: {e}"))?;
        let chunk = page["data"].as_str().unwrap_or("");
        let next = page["next"].as_u64().unwrap_or(from);
        if chunk.is_empty() || next <= from {
            break;
        }
        output.push_str(chunk);
        from = next;
        if output.len() > MAX_TOOL_OUTPUT_BYTES {
            let cut = output.len() - MAX_TOOL_OUTPUT_BYTES;
            // Trim on a char boundary; exactness is not worth a panic.
            let cut = (cut..output.len().min(cut + 4))
                .find(|i| output.is_char_boundary(*i))
                .unwrap_or(0);
            output.drain(..cut);
            truncated = true;
        }
    }
    drop(tty);

    if truncated {
        output = format!("[output truncated to the last {MAX_TOOL_OUTPUT_BYTES} bytes]\n{output}");
    }
    if timed_out {
        return Err(format!(
            "timed out after {timeout}s (the command was killed); output so far:\n{output}"
        ));
    }
    match exit {
        (Some(0), _) => Ok(output),
        (Some(code), _) => Err(format!("exit code {code}\n{output}")),
        (None, Some(sig)) => Err(format!("killed by signal {sig}\n{output}")),
        (None, None) => Ok(output),
    }
}

const SUBAGENT_DEFAULT_TIMEOUT_SECS: u64 = 600;
const SUBAGENT_MAX_TIMEOUT_SECS: u64 = 3600;
/// How deep parentage may go. A subagent may spawn a subagent; that one
/// may not — unbounded recursive spawning is a money fire with no one
/// watching the match.
const MAX_SUBAGENT_DEPTH: usize = 2;

async fn subagent(
    pool: &Pool,
    agent: &Principal,
    project: &str,
    parent_model: &str,
    input: &Value,
) -> Result<String, String> {
    let task = input
        .get("task")
        .and_then(Value::as_str)
        .filter(|t| !t.trim().is_empty())
        .ok_or("subagent needs a non-empty {task}")?;
    let model = input
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(parent_model);
    let timeout = input
        .get("timeout_secs")
        .and_then(Value::as_u64)
        .unwrap_or(SUBAGENT_DEFAULT_TIMEOUT_SECS)
        .clamp(1, SUBAGENT_MAX_TIMEOUT_SECS);
    let Principal::Agent(own_id) = agent else {
        return Err("only an agent can spawn a subagent".into());
    };

    // Depth check: walk the parent chain. Two levels may spawn; deeper may
    // not — the refusal is a result the model reads and works around.
    let mut depth = 0;
    let mut cursor = own_id.clone();
    loop {
        let about = pool
            .call(agent, &cursor, "about", Value::Null)
            .await
            .map_err(|e| format!("cannot inspect the parent chain: {e}"))?;
        match about.get("parent").and_then(Value::as_str) {
            Some(parent) => {
                depth += 1;
                if depth >= MAX_SUBAGENT_DEPTH {
                    return Err(format!(
                        "refused: this chat is already {depth} levels deep — subagents this \
                         deep may not spawn more. Do the task directly."
                    ));
                }
                cursor = parent.to_string();
            }
            None => break,
        }
    }

    let title: String = format!("subagent: {}", task.lines().next().unwrap_or(""))
        .chars()
        .take(48)
        .collect();
    let child = pool
        .create(
            agent,
            "chat",
            project,
            &title,
            json!({"parent": own_id, "model": model}),
        )
        .map_err(|e| format!("cannot create the subagent chat: {e}"))?;

    // The task post is what starts the child's turn (an *other* agent's
    // post triggers — the rule that lets parents task children while a
    // chat still never answers itself).
    pool.call(agent, &child.id, "post", json!({"text": task}))
        .await
        .map_err(|e| format!("cannot post the task: {e}"))?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout);
    let mut mark = 0;
    loop {
        let about = pool
            .call(agent, &child.id, "about", Value::Null)
            .await
            .map_err(|e| format!("subagent chat died: {e}"))?;
        let len = about.get("len").and_then(Value::as_u64).unwrap_or(0);
        if about.get("turn_running") == Some(&json!(false)) && len >= 2 {
            break;
        }
        match tokio::time::timeout_at(deadline, pool.changed(&child.id, mark)).await {
            Ok(Ok(m)) => mark = m,
            Ok(Err(e)) => return Err(format!("subagent chat died: {e}")),
            Err(_) => {
                let _ = pool.call(agent, &child.id, "cancel", Value::Null).await;
                return Err(format!(
                    "subagent {} timed out after {timeout}s (its turn was cancelled); \
                     open the chat to see how far it got",
                    child.id
                ));
            }
        }
    }

    // The answer is the child's final settled assistant entry.
    let tail = pool
        .call(agent, &child.id, "tail", json!({"max_entries": 10_000}))
        .await
        .map_err(|e| format!("subagent chat died: {e}"))?;
    let answer = tail["entries"]
        .as_array()
        .and_then(|entries| {
            entries
                .iter()
                .rev()
                .find(|e| e["t"] == "assistant" && e.get("turn_end").is_some())
        })
        .map(|e| {
            e["content"]
                .as_array()
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter_map(|b| b.get("Text").and_then(|t| t["text"].as_str()))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default()
        })
        .unwrap_or_default();
    if answer.trim().is_empty() {
        return Err(format!(
            "subagent {} settled without a text answer; open the chat to see what happened",
            child.id
        ));
    }
    Ok(format!("subagent {} answered:\n{answer}", child.id))
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

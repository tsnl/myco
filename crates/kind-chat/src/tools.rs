//! The named-tool dispatcher: the model's tools become verbs on the bus,
//! called as the chat's own agent principal. The model never sees a
//! generic `call` — unification is for the substrate, never the prompt
//! (DESIGN.md) — and the bus never sees a privileged channel: the agent
//! creates, drives, and removes instances under exactly the rules any
//! principal lives by.
//!
//! Three tools. `look` is how the model sees the room: a listing, or one
//! object's `recommended_context` (plain text). `bash` runs a command as
//! a one-shot tty instance: create (the agent is creator and driver),
//! wait on the watermark until the child exits or the budget runs out,
//! drain the scrollback, remove. The terminal is a real pool instance
//! while it lives — visible in the tree, watchable by anyone, and cleaned
//! up even when the turn is cancelled mid-command (the removal rides a
//! drop guard). `look` cannot drive; `bash` cannot attach.
//!
//! `subagent` is the ledger's "drop the child machinery" made concrete:
//! it creates an ordinary chat *under* this one — parentage is L1
//! identity, set at birth by the pool — posts the task as this chat's
//! agent principal (another agent's post starts the child's turn — the
//! trigger rule), waits for the child to settle, and splices the answer
//! back. The child chat *remains* in the pool afterwards — subagents are
//! reviewable work, not hidden plumbing. Depth is capped at two levels of
//! parentage, counted from the listing rather than by interrogating each
//! ancestor's kind; the refusal is a result the model reads.

use std::time::Duration;

use myco_instance::{Pool, Principal, VerbError};

use crate::tail;
use myco_models::{ToolResult, ToolSpec, ToolUse};
use serde_json::{Value, json};

/// What the model may call. Handed to the provider at chat create; the
/// dispatcher below is the other half of the same contract.
pub(crate) fn tool_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "look".into(),
            description: "See the room. With no arguments, list every object in this \
                          chat's workspace (kind, title, id, who holds control). With \
                          {title} or {id}, read that object's current text — a terminal's \
                          screen, another chat's transcript. This is how you find a \
                          standing terminal the human opened; bash cannot attach to one."
                .into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "title": {
                        "type": "string",
                        "description": "slug title of the object to read"
                    },
                    "id": {
                        "type": "string",
                        "description": "object id; wins if both id and title are set"
                    }
                }
            }),
        },
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
        "look" => look(pool, agent, project, &tool.input).await,
        "bash" => bash(pool, agent, project, &tool.input).await,
        "subagent" => subagent(pool, agent, project, model, &tool.input).await,
        other => Err(format!(
            "unknown tool {other:?} — available tools: look, bash, subagent"
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
const LOOK_TEXT_CAP: usize = 8 * 1024;

async fn look(
    pool: &Pool,
    agent: &Principal,
    project: &str,
    input: &Value,
) -> Result<String, String> {
    let id = input
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let title = input
        .get("title")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    if id.is_none() && title.is_none() {
        return Ok(list_workspace(pool, project));
    }
    let info = resolve_object(pool, project, id, title)?;
    let verb = recommended_context(pool, &info.kind);
    let payload = pool
        .call(agent, &info.id, verb, Value::Null)
        .await
        .map_err(|e| format!("cannot read {}: {e}", label(&info)))?;
    Ok(format_object(&info, &payload))
}

fn list_workspace(pool: &Pool, project: &str) -> String {
    let rows = pool.list(Some(project));
    if rows.is_empty() {
        return "the workspace is empty.".into();
    }
    let mut out = String::from("workspace objects:\n");
    for info in rows {
        let control = match &info.driver {
            Some(p) => format!("{p}"),
            None => "open".into(),
        };
        let parent = info
            .parent
            .as_deref()
            .map(|p| format!(" parent={p}"))
            .unwrap_or_default();
        out.push_str(&format!(
            "- {kind} {title} id={id} control={control}{parent}\n",
            kind = info.kind,
            title = info.title,
            id = info.id,
            control = control,
            parent = parent,
        ));
    }
    out
}

fn resolve_object(
    pool: &Pool,
    project: &str,
    id: Option<&str>,
    title: Option<&str>,
) -> Result<myco_instance::InstanceInfo, String> {
    let rows = pool.list(Some(project));
    if let Some(id) = id {
        return rows
            .into_iter()
            .find(|i| i.id == id)
            .ok_or_else(|| format!("no object with id {id} in this workspace"));
    }
    let title = title.expect("caller checked");
    let hits: Vec<_> = rows.into_iter().filter(|i| i.title == title).collect();
    match hits.len() {
        0 => Err(format!("no object titled {title:?} in this workspace")),
        1 => Ok(hits.into_iter().next().expect("len 1")),
        n => {
            let ids: Vec<_> = hits.iter().map(|i| i.id.as_str()).collect();
            Err(format!(
                "{n} objects titled {title:?}: {}. Use {{id}}.",
                ids.join(", ")
            ))
        }
    }
}

fn recommended_context(pool: &Pool, kind: &str) -> &'static str {
    pool.kinds()
        .into_iter()
        .find(|k| k.kind == kind)
        .map(|k| k.recommended_context)
        .unwrap_or("text")
}

fn label(info: &myco_instance::InstanceInfo) -> String {
    format!("{} {}", info.kind, info.title)
}

fn format_object(info: &myco_instance::InstanceInfo, payload: &Value) -> String {
    let mut text = payload
        .get("text")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| payload.to_string());
    if text.len() > LOOK_TEXT_CAP {
        let skip = text.len() - LOOK_TEXT_CAP;
        text = format!("[truncated; last {LOOK_TEXT_CAP} bytes]\n{}", &text[skip..]);
    }
    let running = payload.get("running").and_then(Value::as_bool);
    let mut head = format!("{} {} id={}", info.kind, info.title, info.id);
    if let Some(running) = running {
        head.push_str(if running { " running" } else { " exited" });
    }
    format!("{head}\n{text}")
}

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

    // Empty title: L1 mints tty, tty-2, … — a command line is not a slug.
    let info = pool
        .create(
            agent,
            "tty",
            project,
            "",
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
    // The read that ended the wait is the read that carries the exit — one
    // answer, so the status can never come from a different look than the
    // one that saw the child stop.
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
    let Some(text) = exited else {
        return Err(format!(
            "timed out after {timeout}s (the command was killed); output so far:\n{output}"
        ));
    };
    let exit = (
        text.get("exit_code").and_then(Value::as_i64),
        text.get("exit_signal").and_then(Value::as_i64),
    );
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

    // Depth check, read straight off L1's identity: no verb calls, no
    // per-kind parent field to interrogate, and no chance of a chat that
    // lies about where it came from.
    let depth = pool.ancestors(own_id).len();
    if depth >= MAX_SUBAGENT_DEPTH {
        return Err(format!(
            "refused: this chat is already {depth} levels deep — subagents this deep may not \
             spawn more. Do the task directly."
        ));
    }

    let child = pool
        .create_under(
            agent,
            "chat",
            project,
            "",
            json!({"model": model}),
            Some(own_id),
        )
        .map_err(|e| format!("cannot create the subagent chat: {e}"))?;

    // The task post is what starts the child's turn (an *other* agent's
    // post triggers — the rule that lets parents task children while a
    // chat still never answers itself).
    pool.call(agent, &child.id, "post", json!({"text": task}))
        .await
        .map_err(|e| format!("cannot post the task: {e}"))?;

    // Settled: the turn is over and the transcript holds more than the task
    // post that started it.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout);
    let settled = pool
        .wait_until(agent, &child.id, "about", deadline, |about| {
            about.get("turn_running") == Some(&json!(false))
                && about.get("len").and_then(Value::as_u64).unwrap_or(0) >= 2
        })
        .await
        .map_err(|e| format!("subagent chat died: {e}"))?;
    if settled.is_none() {
        let _ = pool.call(agent, &child.id, "cancel", Value::Null).await;
        return Err(format!(
            "subagent {} timed out after {timeout}s (its turn was cancelled); \
             open the chat to see how far it got",
            child.id
        ));
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

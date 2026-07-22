//! Root-only `subagent` tool service (installed on the in-process local worker).

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use uuid::Uuid;

use crate::Harness;
use crate::core::{Async, CancelToken};
use crate::generative_model::{self, Content, GenerativeModelConfig, Message, ModelCatalog};
use crate::prompts;
use crate::session::{
    Agent, AgentEvent, EventSink, Session, SessionKind, TraceContext, uuid_simple_hex,
};
use crate::tool_services::{HostDispatchContext, ToolService};

const SUBAGENT_SYSTEM_PROMPT_PROLOGUE: &str = r#"
You are a subagent being instructed by a supervisor agent.

Instructions from the supervisor arrive as user turns. Respond to each one; the supervisor may
send follow-up instructions in later turns of the same session. Always include your subagent ID
in the response to the supervisor agent.

You share tools with the supervisor agent.
"#;

const SUBAGENT_LOG_DIR: &str = ".myco/subagent-logs";

/// Soft cap on concurrent live subagents per harness (all owners combined).
const MAX_LIVE_SUBAGENTS: usize = 8;

/// Handles the agent harness injects into in-process [`HostDispatchContext::agent_root`].
pub struct AgentRootHandles {
    pub harness: Arc<Harness>,
    pub sink: Arc<dyn EventSink>,
    pub context: TraceContext,
}

/// Spawns subagents that share the supervisor's harness and event sink:
/// one-shot (`run`, the default) or live multi-turn (`start`/`send`/`close`/`list`).
///
/// Only useful when registered on the root in-process local worker (needs
/// [`AgentRootHandles`]). Every subagent persists a **hidden** [`Session`]
/// under `~/.myco/session/` with `id == agent_id` (same hex as runtime
/// correlation), re-saved after every turn, plus a debug transcript at
/// `.myco/subagent-logs/{agent_id}.log`.
///
/// Live subagents are registered under that same id. Exactly one turn may be
/// in flight per live subagent: its [`Agent`] is taken out of the registry for
/// the duration ([`LiveSubagent::runtime`] `None` ⇒ busy). Turns run on a
/// detached task ([`Self::spawn_turn`]) so a supervisor cancel — which drops
/// the awaiting tool future — cannot leave the subagent's history mid-mutation.
///
/// Subagent models come from the same resolved [`ModelCatalog`] as the
/// supervisor (config.toml `[models]`); the advertised tool description lists
/// the configured keys.
#[derive(Default)]
pub struct SubagentService {
    models: ModelCatalog,
    live: Mutex<HashMap<String, LiveSubagent>>,
}

struct LiveSubagent {
    /// Agent that started this subagent; only this agent may send/close it.
    owner: Uuid,
    model_key: String,
    /// Tool use that spawned this subagent (kept for the debug log).
    parent_tool_use_id: String,
    log_path: PathBuf,
    created_at: Instant,
    last_used: Instant,
    /// Completed turns.
    turns: u64,
    /// Taken out while a turn is in flight (`None` ⇒ busy).
    runtime: Option<Runtime>,
}

/// Per-subagent mutable state, owned by exactly one place at a time: the
/// registry entry while idle, the in-flight turn task while a turn runs.
struct Runtime {
    agent: Agent,
    session: Session,
}

/// Everything one subagent turn needs, moved onto the detached turn task.
struct TurnJob {
    subagent_id: String,
    model_key: String,
    parent_tool_use_id: String,
    log_path: PathBuf,
    agent: Agent,
    session: Session,
}

impl SubagentService {
    pub fn new(models: ModelCatalog) -> Self {
        Self {
            models,
            live: Mutex::new(HashMap::new()),
        }
    }
}

impl ToolService for SubagentService {
    fn tool_specs(&self) -> Vec<generative_model::ToolSpec> {
        vec![generative_model::ToolSpec {
            name: "subagent".to_string(),
            description: format!(
                "Runs subagents: one-shot by default, or live multi-turn sessions.\n\n\
                 Actions:\n\
                 - run (default): one-shot subagent. Requires `prompt` and `model`. Blocks until \
                 the subagent replies, then it is gone (though it may use multiple internal turns \
                 for tool calls before replying).\n\
                 - start: spawn a **live** subagent and deliver `prompt` as its first turn. \
                 Requires `prompt` and `model`. The result reports the generated `subagent_id`; \
                 the subagent stays resident for follow-ups (max {MAX_LIVE_SUBAGENTS} live).\n\
                 - send: deliver `prompt` as the next user turn of a live subagent \
                 (`subagent_id`). Blocks until it replies. One turn in flight per subagent — \
                 concurrent sends are rejected while busy.\n\
                 - close: drop a live subagent (`subagent_id`). Its session file remains.\n\
                 - list: list your live subagents.\n\n\
                 Omitting `action` means send when `subagent_id` is set, else run.\n\n\
                 Every subagent persists a session under `~/.myco/session/` with `kind: subagent` \
                 (hidden in default listings) whose id equals the subagent id, re-saved after \
                 every turn — accessible via `session_meta` get-by-id or list with \
                 `include_hidden: true`. Live subagents do not survive the supervisor session; \
                 only the session file does.\n\
                 Configured models for the `model` field: [{}]",
                self.models.keys().join(", ")
            ),
            input_schema: crate::tool_services::tool_input_schema::<Input>(),
        }]
    }

    fn dispatch_tool_use(
        self: Arc<Self>,
        tool_use: generative_model::ToolUse,
        ctx: HostDispatchContext,
    ) -> Async<generative_model::ToolResult> {
        Box::pin(async move {
            let input: Input = match serde_json::from_value(tool_use.input) {
                Ok(input) => input,
                Err(e) => {
                    return generative_model::ToolResult::err(format!(
                        "Error deserializing subagent input: {e}"
                    ));
                }
            };
            let action = match resolve_action(input) {
                Ok(a) => a,
                Err(e) => return generative_model::ToolResult::err(e),
            };
            self.execute(action, tool_use.id, ctx).await
        })
    }

    fn on_agent_finished(&self, agent_id: Uuid) {
        self.reap_owner(agent_id);
    }
}

impl SubagentService {
    async fn execute(
        self: Arc<Self>,
        action: Action,
        parent_tool_use_id: String,
        ctx: HostDispatchContext,
    ) -> generative_model::ToolResult {
        match action {
            Action::Run { prompt, model } => {
                self.run_new(prompt, model, parent_tool_use_id, ctx, false)
                    .await
            }
            Action::Start { prompt, model } => {
                self.run_new(prompt, model, parent_tool_use_id, ctx, true)
                    .await
            }
            Action::Send {
                subagent_id,
                prompt,
            } => self.send(subagent_id, prompt, ctx).await,
            Action::Close { subagent_id } => self.close(&subagent_id, ctx.agent_id),
            Action::List => self.list(ctx.agent_id),
        }
    }

    /// Spawn a fresh subagent and run its first turn. With `live`, register it
    /// for follow-up `send` turns; otherwise it is dropped after replying.
    async fn run_new(
        self: Arc<Self>,
        prompt: String,
        model_key: String,
        parent_tool_use_id: String,
        ctx: HostDispatchContext,
        live: bool,
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

        // Cheap cap check before building anything; re-checked under the lock
        // at registration (concurrent starts in one supervisor turn).
        if live && self.live_len() >= MAX_LIVE_SUBAGENTS {
            return generative_model::ToolResult::err(format!(
                "too many live subagents (max {MAX_LIVE_SUBAGENTS}); close one first"
            ));
        }

        let catalog_model = match self.models.get(&model_key) {
            Ok(m) => m,
            Err(e) => return generative_model::ToolResult::err(e),
        };
        let model = match generative_model::new(GenerativeModelConfig {
            model: catalog_model.spec.clone(),
            tools: root.harness.tool_specs(),
            system_prompt: [
                SUBAGENT_SYSTEM_PROMPT_PROLOGUE.to_string(),
                prompts::agent_prompt_epilogue(),
            ]
            .join("\n\n"),
            backend_config: catalog_model.backend.clone(),
        }) {
            Ok(m) => m,
            Err(e) => {
                return generative_model::ToolResult::err(format!(
                    "Failed to create subagent model: {e:?}"
                ));
            }
        };

        let agent_id = Uuid::new_v4();
        let subagent_id = uuid_simple_hex(agent_id);
        let log_path = PathBuf::from(SUBAGENT_LOG_DIR).join(format!("{subagent_id}.log"));

        if live {
            let mut map = match self.live.lock() {
                Ok(g) => g,
                Err(e) => {
                    return generative_model::ToolResult::err(format!(
                        "live subagents lock poisoned: {e}"
                    ));
                }
            };
            if map.len() >= MAX_LIVE_SUBAGENTS {
                return generative_model::ToolResult::err(format!(
                    "too many live subagents (max {MAX_LIVE_SUBAGENTS}); close one first"
                ));
            }
            // Registered busy (no runtime yet); the first turn's task fills it.
            map.insert(
                subagent_id.clone(),
                LiveSubagent {
                    owner: ctx.agent_id,
                    model_key: model_key.clone(),
                    parent_tool_use_id: parent_tool_use_id.clone(),
                    log_path: log_path.clone(),
                    created_at: Instant::now(),
                    last_used: Instant::now(),
                    turns: 0,
                    runtime: None,
                },
            );
        }

        let parent_session_id = uuid_simple_hex(root.context.agent_id);
        let mut session = Session::new_hidden(
            model_key.as_str(),
            subagent_id.clone(),
            SessionKind::Subagent,
            Some(parent_session_id.clone()),
        );
        session.title = Some(format!("subagent of {parent_session_id}"));
        if let Err(e) = session.save() {
            eprintln!("warning: failed to create hidden subagent session {subagent_id}: {e}");
        }

        let child_context = root
            .context
            .child_agent(agent_id, Some(parent_tool_use_id.clone()));

        root.sink.emit(AgentEvent::AgentStarted {
            agent_id,
            model: model_key.clone(),
            parent_agent_id: Some(root.context.agent_id),
            parent_tool_use_id: Some(parent_tool_use_id.clone()),
            depth: child_context.depth,
        });

        let agent = Agent::with_context(
            model,
            root.harness.clone(),
            root.sink.clone(),
            child_context,
        );

        let id_notice = format!(
            "Your subagent UUID is {subagent_id} (hidden session id). \
             Write durable details to `{}` if needed; the harness also persists \
             this session under ~/.myco/session/ and a debug log at that path.",
            log_path.display()
        );

        let handle = self.clone().spawn_turn(
            TurnJob {
                subagent_id: subagent_id.clone(),
                model_key,
                parent_tool_use_id,
                log_path,
                agent,
                session,
            },
            vec![
                Content::Text {
                    text: prompt.clone(),
                },
                Content::Text { text: id_notice },
            ],
            prompt,
            ctx.cancel.clone(),
        );

        match handle.await {
            Ok(Ok(text)) if live => {
                let body = if text.is_empty() {
                    format!("(subagent {subagent_id} replied with no text)")
                } else {
                    text
                };
                generative_model::ToolResult::text(format!(
                    "{body}\n\n(live subagent_id={subagent_id}; continue with action=send, \
                     end with action=close)"
                ))
            }
            Ok(Ok(text)) if text.is_empty() => generative_model::ToolResult::text(format!(
                "(subagent {subagent_id} finished with no text; hidden session={subagent_id})"
            )),
            Ok(Ok(text)) => generative_model::ToolResult::text(format!(
                "{text}\n\n(subagent session={subagent_id})"
            )),
            Ok(Err(e)) if live => generative_model::ToolResult::err(format!(
                "{e}\n(subagent {subagent_id} still live; retry with send or close it)"
            )),
            Ok(Err(e)) => generative_model::ToolResult::err(e),
            Err(e) => generative_model::ToolResult::err(format!(
                "subagent {subagent_id} turn task failed: {e}"
            )),
        }
    }

    /// Deliver `prompt` as the next user turn of a live subagent and wait for
    /// its reply.
    async fn send(
        self: Arc<Self>,
        subagent_id: String,
        prompt: String,
        ctx: HostDispatchContext,
    ) -> generative_model::ToolResult {
        let job = {
            let mut live = match self.live.lock() {
                Ok(g) => g,
                Err(e) => {
                    return generative_model::ToolResult::err(format!(
                        "live subagents lock poisoned: {e}"
                    ));
                }
            };
            let Some(entry) = live.get_mut(&subagent_id) else {
                return generative_model::ToolResult::err(format!(
                    "unknown live subagent {subagent_id:?} (start one, or list yours)"
                ));
            };
            if entry.owner != ctx.agent_id {
                return generative_model::ToolResult::err(format!(
                    "subagent {subagent_id:?} is owned by another agent"
                ));
            }
            let Some(runtime) = entry.runtime.take() else {
                return generative_model::ToolResult::err(format!(
                    "subagent {subagent_id:?} is busy with an in-flight turn; \
                     wait for it to finish"
                ));
            };
            entry.last_used = Instant::now();
            TurnJob {
                subagent_id: subagent_id.clone(),
                model_key: entry.model_key.clone(),
                parent_tool_use_id: entry.parent_tool_use_id.clone(),
                log_path: entry.log_path.clone(),
                agent: runtime.agent,
                session: runtime.session,
            }
        };

        let handle = self.clone().spawn_turn(
            job,
            vec![Content::Text {
                text: prompt.clone(),
            }],
            prompt,
            ctx.cancel.clone(),
        );

        match handle.await {
            Ok(Ok(text)) => {
                let body = if text.is_empty() {
                    format!("(subagent {subagent_id} replied with no text)")
                } else {
                    text
                };
                generative_model::ToolResult::text(format!("{body}\n\n(subagent_id={subagent_id})"))
            }
            Ok(Err(e)) => generative_model::ToolResult::err(format!(
                "{e}\n(subagent {subagent_id} still live; retry with send or close it)"
            )),
            Err(e) => generative_model::ToolResult::err(format!(
                "subagent {subagent_id} turn task failed: {e}"
            )),
        }
    }

    /// Run one subagent turn on a detached task and return its join handle.
    ///
    /// Detached on purpose: the supervisor's cancel drops the awaiting tool
    /// future (`Agent::dispatch_tool_use` races cancel vs work), and dropping
    /// `interact` mid-turn would abandon the history mid-mutation (e.g. a
    /// dangling tool_use with no results). On the task, `interact` observes
    /// the same cancel token, returns promptly with well-formed history, and
    /// the state is persisted and returned to the registry whether or not
    /// anyone is still awaiting the result.
    fn spawn_turn(
        self: Arc<Self>,
        mut job: TurnJob,
        content: Vec<Content>,
        prompt: String,
        cancel: CancelToken,
    ) -> tokio::task::JoinHandle<Result<String, String>> {
        tokio::spawn(async move {
            let interact_result = job.agent.interact(content, cancel).await;

            job.session.messages = job.agent.history().to_vec();
            job.session.touch();
            if let Err(e) = job.session.save() {
                eprintln!(
                    "warning: failed to save hidden subagent session {}: {e}",
                    job.subagent_id
                );
            }

            if let Err(e) = write_subagent_log(
                &job.log_path,
                &job.subagent_id,
                &job.model_key,
                Some(&job.parent_tool_use_id),
                &prompt,
                job.agent.history(),
                interact_result.as_ref().ok().map(|v| v.as_slice()),
                interact_result.as_ref().err().map(|e| e.to_string()),
            ) {
                eprintln!(
                    "warning: failed to write subagent log {}: {e}",
                    job.log_path.display()
                );
            }

            let outcome = match &interact_result {
                Ok(content) => Ok(content
                    .iter()
                    .filter_map(|c| match c {
                        Content::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n")),
                Err(e) => Err(format!("subagent {} failed: {e}", job.subagent_id)),
            };

            self.finish_turn(job);
            outcome
        })
    }

    /// Return a turn's state to the registry. A subagent closed or reaped
    /// mid-turn (entry gone — including one-shot `run`, which never registers)
    /// is dropped here instead, outside the registry lock: dropping an
    /// [`Agent`] cascades cleanup that can re-enter this service.
    fn finish_turn(&self, job: TurnJob) {
        let leftover = {
            let Ok(mut live) = self.live.lock() else {
                return;
            };
            match live.get_mut(&job.subagent_id) {
                Some(entry) => {
                    entry.turns += 1;
                    entry.last_used = Instant::now();
                    entry.runtime = Some(Runtime {
                        agent: job.agent,
                        session: job.session,
                    });
                    None
                }
                None => Some(job),
            }
        };
        drop(leftover);
    }

    fn close(&self, subagent_id: &str, owner: Uuid) -> generative_model::ToolResult {
        let removed = {
            let mut live = match self.live.lock() {
                Ok(g) => g,
                Err(e) => {
                    return generative_model::ToolResult::err(format!(
                        "live subagents lock poisoned: {e}"
                    ));
                }
            };
            match live.get(subagent_id) {
                Some(e) if e.owner != owner => {
                    return generative_model::ToolResult::err(format!(
                        "subagent {subagent_id:?} is owned by another agent"
                    ));
                }
                Some(_) => {}
                None => {
                    return generative_model::ToolResult::err(format!(
                        "unknown live subagent {subagent_id:?}"
                    ));
                }
            }
            live.remove(subagent_id).expect("entry present after check")
        };
        let busy = removed.runtime.is_none();
        let turns = removed.turns;
        // Drop outside the lock: Agent::drop cascades cleanup (its bash
        // sessions, its own live subagents) that can re-enter this service.
        drop(removed);
        let note = if busy {
            "; in-flight turn will finish and persist the session first"
        } else {
            ""
        };
        generative_model::ToolResult::text(format!(
            "(subagent {subagent_id} closed after {turns} turns; session persisted{note})\n"
        ))
    }

    fn list(&self, owner: Uuid) -> generative_model::ToolResult {
        let live = match self.live.lock() {
            Ok(g) => g,
            Err(e) => {
                return generative_model::ToolResult::err(format!(
                    "live subagents lock poisoned: {e}"
                ));
            }
        };
        let mut mine: Vec<_> = live.iter().filter(|(_, e)| e.owner == owner).collect();
        if mine.is_empty() {
            return generative_model::ToolResult::text("(no live subagents)\n");
        }
        mine.sort_by(|a, b| a.0.cmp(b.0));
        let mut lines = Vec::new();
        lines.push(format!("live subagents: {}", mine.len()));
        for (id, e) in mine {
            let status = if e.runtime.is_some() { "idle" } else { "busy" };
            lines.push(format!(
                "- subagent_id={id} model={} status={status} turns={} last_used_s_ago={} created_s_ago={}",
                e.model_key,
                e.turns,
                e.last_used.elapsed().as_secs(),
                e.created_at.elapsed().as_secs(),
            ));
        }
        lines.push(String::new());
        generative_model::ToolResult::text(lines.join("\n"))
    }

    /// Synchronously drop every live subagent owned by `owner`.
    /// Called from `on_agent_finished` / `Agent::drop` — must not await.
    ///
    /// Victims drop outside the registry lock: an [`Agent`] drop cascades
    /// cleanup, and a nested subagent's reap re-enters this map.
    fn reap_owner(&self, owner: Uuid) {
        let victims: Vec<LiveSubagent> = {
            let Ok(mut live) = self.live.lock() else {
                return;
            };
            let keys: Vec<String> = live
                .iter()
                .filter(|(_, e)| e.owner == owner)
                .map(|(id, _)| id.clone())
                .collect();
            keys.into_iter().filter_map(|id| live.remove(&id)).collect()
        };
        drop(victims);
    }

    fn live_len(&self) -> usize {
        self.live.lock().map(|g| g.len()).unwrap_or(0)
    }
}

/// Debug transcript for one subagent, rewritten in full after every turn
/// (`prompt` is the latest turn's instruction; `history` is complete).
#[allow(clippy::too_many_arguments)]
fn write_subagent_log(
    path: &Path,
    subagent_id: &str,
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
    writeln!(file, "agent_id={subagent_id}").map_err(|e| e.to_string())?;
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

// --- input schema ------------------------------------------------------------

/// Wire input for the `subagent` tool (flat object; see [`resolve_action`]).
#[derive(
    Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize, PartialEq, Eq,
)]
struct Input {
    /// Action to perform. Omitted ⇒ `send` when `subagent_id` is set, else `run`.
    #[serde(default)]
    action: Option<ActionKind>,
    /// Instruction for the subagent — one user turn (`run` / `start` / `send`).
    #[serde(default)]
    prompt: Option<String>,
    /// Model key for a new subagent (`run` / `start`) — one of the configured
    /// catalog keys (listed in the tool description).
    #[serde(default)]
    model: Option<String>,
    /// Live subagent id, as reported by `start` (`send` / `close`).
    #[serde(default)]
    subagent_id: Option<String>,
}

#[derive(
    Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize, PartialEq, Eq,
)]
#[serde(rename_all = "snake_case")]
enum ActionKind {
    Run,
    Start,
    Send,
    Close,
    List,
}

/// Internal validated action after parsing [`Input`].
#[derive(Debug)]
enum Action {
    Run { prompt: String, model: String },
    Start { prompt: String, model: String },
    Send { subagent_id: String, prompt: String },
    Close { subagent_id: String },
    List,
}

fn resolve_action(input: Input) -> Result<Action, String> {
    let kind = match input.action {
        Some(k) => k,
        // Bare `{prompt, model}` stays one-shot; a `subagent_id` means a live target.
        None if input.subagent_id.is_some() => ActionKind::Send,
        None => ActionKind::Run,
    };
    match kind {
        ActionKind::Run => Ok(Action::Run {
            prompt: input.prompt.ok_or("run requires `prompt`")?,
            model: input.model.ok_or("run requires `model`")?,
        }),
        ActionKind::Start => Ok(Action::Start {
            prompt: input.prompt.ok_or("start requires `prompt`")?,
            model: input.model.ok_or("start requires `model`")?,
        }),
        ActionKind::Send => Ok(Action::Send {
            subagent_id: input.subagent_id.ok_or("send requires `subagent_id`")?,
            prompt: input.prompt.ok_or("send requires `prompt`")?,
        }),
        ActionKind::Close => Ok(Action::Close {
            subagent_id: input.subagent_id.ok_or("close requires `subagent_id`")?,
        }),
        ActionKind::List => Ok(Action::List),
    }
}

#[cfg(test)]
impl SubagentService {
    fn insert_live_for_test(&self, subagent_id: &str, owner: Uuid, agent: Agent, session: Session) {
        self.live.lock().expect("live lock").insert(
            subagent_id.to_string(),
            LiveSubagent {
                owner,
                model_key: "scripted".into(),
                parent_tool_use_id: "test_parent".into(),
                log_path: std::env::temp_dir()
                    .join(format!("myco-test-subagent-{subagent_id}.log")),
                created_at: Instant::now(),
                last_used: Instant::now(),
                turns: 0,
                runtime: Some(Runtime { agent, session }),
            },
        );
    }

    /// Take a live subagent's runtime out, as an in-flight turn would.
    fn take_runtime_for_test(&self, subagent_id: &str) -> Option<Runtime> {
        self.live
            .lock()
            .expect("live lock")
            .get_mut(subagent_id)
            .and_then(|e| e.runtime.take())
    }

    fn live_len_for_test(&self) -> usize {
        self.live.lock().expect("live lock").len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generative_model::{
        ContentDelta, ContentStart, GenerateError, GenerativeModel, MessagePart, TurnEndReason,
    };
    use crate::session::NullEventSink;
    use serde_json::json;
    use std::collections::VecDeque;

    /// Yields one scripted text reply per generate call (no tool use) and
    /// records the history length seen by each call.
    struct ScriptedReplies {
        replies: Mutex<VecDeque<String>>,
        seen_input_lens: Arc<Mutex<Vec<usize>>>,
    }

    impl ScriptedReplies {
        fn new(replies: &[&str]) -> Arc<Self> {
            Arc::new(Self {
                replies: Mutex::new(replies.iter().map(|s| s.to_string()).collect()),
                seen_input_lens: Arc::new(Mutex::new(Vec::new())),
            })
        }
    }

    impl GenerativeModel for ScriptedReplies {
        fn generate(
            &self,
            input: &[Message],
        ) -> crate::core::AsyncStream<Result<MessagePart, GenerateError>> {
            self.seen_input_lens
                .lock()
                .expect("seen lock")
                .push(input.len());
            let text = self
                .replies
                .lock()
                .expect("replies lock")
                .pop_front()
                .expect("scripted replies exhausted");
            let parts = vec![
                MessagePart::MessageStart,
                MessagePart::ContentStart(ContentStart::Text { index: 0 }),
                MessagePart::ContentDelta(ContentDelta::Text {
                    index: 0,
                    delta: text,
                }),
                MessagePart::TurnEndReason(TurnEndReason::EndTurn),
            ];
            Box::pin(futures::stream::iter(parts.into_iter().map(Ok)))
        }
    }

    fn service() -> Arc<SubagentService> {
        Arc::new(SubagentService::new(ModelCatalog::default()))
    }

    fn insert_scripted(
        service: &SubagentService,
        id: &str,
        owner: Uuid,
        model: Arc<ScriptedReplies>,
    ) {
        let harness = Harness::local_with_services(vec![]);
        let agent = Agent::with_context(
            model,
            harness,
            Arc::new(NullEventSink),
            TraceContext::root().child_agent(Uuid::new_v4(), None),
        );
        let session = Session::new_hidden("scripted", id.to_string(), SessionKind::Subagent, None);
        service.insert_live_for_test(id, owner, agent, session);
    }

    async fn dispatch(
        service: &Arc<SubagentService>,
        agent_id: Uuid,
        input: serde_json::Value,
    ) -> generative_model::ToolResult {
        Arc::clone(service)
            .dispatch_tool_use(
                generative_model::ToolUse {
                    id: "tu_test".into(),
                    name: "subagent".into(),
                    input,
                },
                HostDispatchContext::bare(agent_id, CancelToken::new()),
            )
            .await
    }

    fn result_text(result: &generative_model::ToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|c| match c {
                Content::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn action_inference_and_field_requirements() {
        // Bare `{prompt, model}` stays one-shot (pre-multi-turn wire compat).
        let input: Input = serde_json::from_value(json!({"prompt": "p", "model": "m"})).unwrap();
        assert!(matches!(resolve_action(input).unwrap(), Action::Run { .. }));

        // A subagent_id without an action means a follow-up send.
        let input: Input =
            serde_json::from_value(json!({"subagent_id": "s", "prompt": "p"})).unwrap();
        assert!(matches!(
            resolve_action(input).unwrap(),
            Action::Send { .. }
        ));

        // Missing fields fail loud, per action.
        for (value, needle) in [
            (json!({}), "run requires `prompt`"),
            (
                json!({"action": "run", "prompt": "p"}),
                "run requires `model`",
            ),
            (
                json!({"action": "start", "model": "m"}),
                "start requires `prompt`",
            ),
            (
                json!({"action": "send", "prompt": "p"}),
                "send requires `subagent_id`",
            ),
            (
                json!({"action": "send", "subagent_id": "s"}),
                "send requires `prompt`",
            ),
            (json!({"action": "close"}), "close requires `subagent_id`"),
        ] {
            let input: Input = serde_json::from_value(value.clone()).unwrap();
            let err = resolve_action(input).unwrap_err();
            assert!(err.contains(needle), "input={value} err={err}");
        }
    }

    /// The tool description is the model-facing contract: it must name every
    /// action and the live cap actually enforced.
    #[test]
    fn tool_description_states_actions_and_cap() {
        let specs = SubagentService::new(ModelCatalog::default()).tool_specs();
        let d = &specs[0].description;
        for needle in ["run (default)", "start:", "send:", "close:", "list:"] {
            assert!(d.contains(needle), "description missing {needle}: {d}");
        }
        let cap = MAX_LIVE_SUBAGENTS.to_string();
        assert!(d.contains(&cap), "description missing cap {cap}: {d}");
    }

    #[tokio::test]
    async fn send_rejects_unknown_foreign_and_busy_subagents() {
        let service = service();
        let owner = Uuid::new_v4();
        insert_scripted(&service, "guarded", owner, ScriptedReplies::new(&[]));

        let r = dispatch(
            &service,
            owner,
            json!({"action": "send", "subagent_id": "nope", "prompt": "hi"}),
        )
        .await;
        assert!(r.is_error);
        assert!(result_text(&r).contains("unknown live subagent"), "{r:?}");

        let other = Uuid::new_v4();
        let r = dispatch(
            &service,
            other,
            json!({"action": "send", "subagent_id": "guarded", "prompt": "hi"}),
        )
        .await;
        assert!(r.is_error);
        assert!(result_text(&r).contains("owned by another agent"), "{r:?}");

        // Simulate an in-flight turn: runtime is out of the registry.
        let taken = service.take_runtime_for_test("guarded").expect("runtime");
        let r = dispatch(
            &service,
            owner,
            json!({"action": "send", "subagent_id": "guarded", "prompt": "hi"}),
        )
        .await;
        assert!(r.is_error);
        assert!(result_text(&r).contains("busy"), "{r:?}");
        drop(taken);
    }

    // Deliberate guard-across-await: it serializes MYCO_HOME for the whole
    // test, and #[tokio::test] runs on a current-thread runtime.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn send_delivers_follow_up_turns_on_one_growing_history() {
        let _guard = crate::session::lock_myco_home_for_test();
        let dir = std::env::temp_dir().join(format!(
            "myco-subagent-multiturn-{}",
            uuid_simple_hex(Uuid::new_v4())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: test-only env override; held under the myco-home lock.
        unsafe {
            std::env::set_var("MYCO_HOME", &dir);
        }

        let service = service();
        let owner = Uuid::new_v4();
        let model = ScriptedReplies::new(&["first reply", "second reply"]);
        let seen = model.seen_input_lens.clone();
        insert_scripted(&service, "livesub1", owner, model);

        let r1 = dispatch(
            &service,
            owner,
            json!({"action": "send", "subagent_id": "livesub1", "prompt": "hi"}),
        )
        .await;
        assert!(!r1.is_error, "{r1:?}");
        let text = result_text(&r1);
        assert!(text.contains("first reply"), "{text}");
        assert!(text.contains("subagent_id=livesub1"), "{text}");

        // Inferred send: subagent_id without an action.
        let r2 = dispatch(
            &service,
            owner,
            json!({"subagent_id": "livesub1", "prompt": "again"}),
        )
        .await;
        assert!(!r2.is_error, "{r2:?}");
        assert!(
            result_text(&r2).contains("second reply"),
            "{}",
            result_text(&r2)
        );

        // One shared history across turns: 1 message at the first generate
        // (user), 3 at the second (user, assistant, user).
        assert_eq!(*seen.lock().unwrap(), vec![1, 3]);

        // Idle again with both turns counted, and the hidden session holds the
        // full conversation.
        let list = dispatch(&service, owner, json!({"action": "list"})).await;
        let list_text = result_text(&list);
        assert!(list_text.contains("subagent_id=livesub1"), "{list_text}");
        assert!(list_text.contains("status=idle"), "{list_text}");
        assert!(list_text.contains("turns=2"), "{list_text}");

        let session = Session::load_by_id_or_prefix("livesub1").expect("saved session");
        assert_eq!(session.kind, SessionKind::Subagent);
        assert_eq!(session.messages.len(), 4);

        let _ = std::fs::remove_dir_all(&dir);
        unsafe {
            std::env::remove_var("MYCO_HOME");
        }
    }

    #[tokio::test]
    async fn close_removes_live_subagent_and_later_send_fails() {
        let service = service();
        let owner = Uuid::new_v4();
        insert_scripted(&service, "doomed", owner, ScriptedReplies::new(&[]));

        let r = dispatch(
            &service,
            owner,
            json!({"action": "close", "subagent_id": "doomed"}),
        )
        .await;
        assert!(!r.is_error, "{r:?}");
        assert!(result_text(&r).contains("closed"), "{}", result_text(&r));
        assert_eq!(service.live_len_for_test(), 0);

        let r = dispatch(
            &service,
            owner,
            json!({"action": "send", "subagent_id": "doomed", "prompt": "hi"}),
        )
        .await;
        assert!(r.is_error);
        assert!(result_text(&r).contains("unknown live subagent"), "{r:?}");
    }

    /// A subagent closed while its turn is in flight must not resurrect when
    /// the turn task returns its state: `finish_turn` drops it instead.
    #[tokio::test]
    async fn close_while_busy_detaches_and_turn_state_is_dropped() {
        let service = service();
        let owner = Uuid::new_v4();
        insert_scripted(&service, "midflight", owner, ScriptedReplies::new(&[]));
        let runtime = service.take_runtime_for_test("midflight").expect("runtime");

        let r = dispatch(
            &service,
            owner,
            json!({"action": "close", "subagent_id": "midflight"}),
        )
        .await;
        assert!(!r.is_error, "{r:?}");
        assert!(result_text(&r).contains("in-flight"), "{}", result_text(&r));
        assert_eq!(service.live_len_for_test(), 0);

        // The in-flight turn completes and hands its state back.
        service.finish_turn(TurnJob {
            subagent_id: "midflight".into(),
            model_key: "scripted".into(),
            parent_tool_use_id: "test_parent".into(),
            log_path: std::env::temp_dir().join("myco-test-subagent-midflight.log"),
            agent: runtime.agent,
            session: runtime.session,
        });
        assert_eq!(service.live_len_for_test(), 0);
    }

    #[tokio::test]
    async fn agent_finished_reaps_only_that_owners_subagents() {
        let service = service();
        let owner_a = Uuid::new_v4();
        let owner_b = Uuid::new_v4();
        insert_scripted(&service, "of-a", owner_a, ScriptedReplies::new(&[]));
        insert_scripted(&service, "of-b", owner_b, ScriptedReplies::new(&[]));

        service.on_agent_finished(owner_a);
        assert_eq!(service.live_len_for_test(), 1);
        let list = dispatch(&service, owner_b, json!({"action": "list"})).await;
        assert!(result_text(&list).contains("of-b"), "{list:?}");
    }

    #[tokio::test]
    async fn start_beyond_live_cap_is_rejected_before_building_anything() {
        let service = service();
        let owner = Uuid::new_v4();
        for i in 0..MAX_LIVE_SUBAGENTS {
            insert_scripted(
                &service,
                &format!("cap{i}"),
                owner,
                ScriptedReplies::new(&[]),
            );
        }

        let harness = Harness::local_with_services(vec![]);
        let root = AgentRootHandles {
            harness,
            sink: Arc::new(NullEventSink),
            context: TraceContext::root(),
        };
        let result = Arc::clone(&service)
            .dispatch_tool_use(
                generative_model::ToolUse {
                    id: "tu_cap".into(),
                    name: "subagent".into(),
                    input: json!({"action": "start", "prompt": "x", "model": "any"}),
                },
                HostDispatchContext {
                    agent_id: owner,
                    cancel: CancelToken::new(),
                    agent_root: Some(Arc::new(root)),
                },
            )
            .await;
        assert!(result.is_error);
        assert!(
            result_text(&result).contains("too many live subagents"),
            "{result:?}"
        );
        assert_eq!(service.live_len_for_test(), MAX_LIVE_SUBAGENTS);
    }

    #[tokio::test]
    async fn start_without_root_handles_is_rejected() {
        let service = service();
        let r = dispatch(
            &service,
            Uuid::new_v4(),
            json!({"action": "start", "prompt": "x", "model": "any"}),
        )
        .await;
        assert!(r.is_error);
        assert!(result_text(&r).contains("agent root handles"), "{r:?}");
    }
}

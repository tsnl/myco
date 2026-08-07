//! The `chat` kind: a conversation as an instance.
//!
//! v2's "session" dissolves here — a chat is an ordinary instance in the
//! pool, created by anyone, watched like anything else. A **subagent** is
//! nothing but a chat whose `parent` names another chat; there is no child
//! machinery anywhere. The transcript is shared, attributed, and
//! multiplayer: `post` appends, `tail` cursors, and the author is always
//! the authenticated [`Principal`] the bus resolved — never anything the
//! arguments claim.
//!
//! **The turn engine.** A chat created with a `model` answers human posts.
//! A model *turn* is not a mailbox command (DESIGN.md): it runs as a
//! cancellable side-feed task that streams into the transcript through the
//! same shared-state pattern the tty's pty pump uses — verbs stay short,
//! the stream flows outside the cell, and every delta bumps the watermark
//! so watchers re-read. `cancel` aborts the task mid-stream; a human post
//! during a turn is an *interjection* — the running turn dies marked
//! `interrupted` and a fresh turn starts over the longer transcript. The
//! partial entry stays: state is not history, and a cancelled thought
//! visibly happened.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use myco_instance::{Instance, Kind, KindSpec, Pool, Principal, VerbError, VerbSpec};
use myco_models::{
    Content, GenerateOutput, GenerativeModel, GenerativeModelConfig, MessagePart, ModelCatalog,
    ToolResult, ToolUse, TurnEndReason,
};
use myco_runtime::Signals;
use serde_json::{Value, json};

mod project;
pub use project::project;

mod tools;

static CHAT_SPEC: KindSpec = KindSpec {
    kind: "chat",
    version: 1,
    doc: "a conversation: an attributed transcript shared by humans and agents; \
          with a model, posts start turns",
    verbs: &[
        VerbSpec::write(
            "post",
            "append a message {text}; the author is the caller, always. A human post to a \
             modeled chat starts a turn (interrupting a running one)",
        ),
        VerbSpec::write(
            "cancel",
            "abort the running turn, if any; the partial entry stays, marked cancelled",
        ),
        VerbSpec::cursored_read(
            "tail",
            "entries from sequence {from}, at most {max_entries} (default 200); returns {next} \
             and the total {len}",
        ),
        VerbSpec::read(
            "text",
            "the transcript as plain text, one `author: message` line per entry",
        ),
        VerbSpec::write(
            "watch",
            "add a standing subscription: {instance, verb?, args?} — before each generation, \
             changes since the last look are spliced into the transcript as watched entries. \
             The verb must be cursored (default: the target's first cursored verb); watching \
             starts from now",
        ),
        VerbSpec::write("unwatch", "drop the standing subscription on {instance}"),
        VerbSpec::read(
            "about",
            "the chat's shape: {parent, model, len, turn_running, watching}",
        ),
    ],
    primary_render: "tail",
    recommended_context: "text",
};

const DEFAULT_TAIL: u64 = 200;

/// The interim system prompt. The full prompt assembly (tool guidance,
/// workspace context) arrives with the tool dispatcher.
const SYSTEM_PROMPT: &str = "You are the resident agent of a myco chat. Answer the humans in \
                             the transcript plainly and concretely. Messages prefixed with a \
                             bracketed name were said by that person.";

/// One transcript entry. `seq` is dense from 0, so `tail`'s cursor is also
/// an entry count; `body` is tagged (`"t"`) so readers skip variants they
/// don't know.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    pub seq: u64,
    pub at: DateTime<Utc>,
    pub author: Principal,
    #[serde(flatten)]
    pub body: Body,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Body {
    /// A plain post (human or otherwise).
    Message { text: String },
    /// One model turn. `turn_end: None` while the turn is streaming — a
    /// client rendering it is rendering live state, not history.
    Assistant {
        model: String,
        content: Vec<Content>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_uses: Vec<ToolUse>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_end: Option<TurnEndReason>,
    },
    /// The answers to the preceding assistant entry's tool calls, paired
    /// positionally (result j answers tool_use j).
    ToolResults { results: Vec<ToolResult> },
    /// What a standing subscription saw: the delta a cursored read on
    /// `instance` returned since the last look. A real entry, on purpose —
    /// the transcript is the record of what the agent knew, and hidden
    /// context would break that.
    Watched { instance: String, data: String },
}

impl Body {
    fn plain(&self) -> String {
        match self {
            Body::Message { text } => text.clone(),
            Body::Assistant { content, .. } => myco_models::content_text(content),
            Body::ToolResults { results } => results
                .iter()
                .map(|r| myco_models::content_text(&r.content))
                .collect::<Vec<_>>()
                .join("\n"),
            Body::Watched { instance, data } => format!("[watched {instance}]\n{data}"),
        }
    }
}

// ---------------------------------------------------------------------------
// The kind
// ---------------------------------------------------------------------------

/// Builds a live provider from a resolved config. Injected so tests script
/// turns without a network; the default is [`myco_models::new`].
pub type ModelFactory =
    Arc<dyn Fn(GenerativeModelConfig) -> Result<Arc<dyn GenerativeModel>, String> + Send + Sync>;

pub struct ChatKind {
    /// The bus, held as a factory field — the blessed dependency injection
    /// for kinds that act on it (the tool dispatcher creates and drives
    /// tty instances as the chat's agent principal).
    pool: Pool,
    catalog: Arc<ModelCatalog>,
    /// Applied when create args omit `model`. `None` + empty catalog is the
    /// modelless workspace: chats are pure transcripts.
    default_model: Option<String>,
    factory: ModelFactory,
}

impl ChatKind {
    pub fn new(pool: Pool, catalog: ModelCatalog, default_model: Option<String>) -> Self {
        Self::with_factory(
            pool,
            catalog,
            default_model,
            Arc::new(|config| myco_models::new(config).map_err(|e| e.to_string())),
        )
    }

    /// A modelless chat kind: every chat is a pure transcript.
    pub fn transcript_only(pool: Pool) -> Self {
        Self::new(pool, ModelCatalog::default(), None)
    }

    /// The test seam: a factory that answers with a scripted model.
    pub fn with_factory(
        pool: Pool,
        catalog: ModelCatalog,
        default_model: Option<String>,
        factory: ModelFactory,
    ) -> Self {
        Self {
            pool,
            catalog: Arc::new(catalog),
            default_model,
            factory,
        }
    }
}

impl Kind for ChatKind {
    fn spec(&self) -> &'static KindSpec {
        &CHAT_SPEC
    }

    fn create(
        &self,
        ctx: &myco_instance::CreateCtx,
        args: Value,
        _signals: Signals,
    ) -> Result<Box<dyn Instance>, VerbError> {
        let parent = match args.get("parent") {
            None | Some(Value::Null) => None,
            Some(Value::String(id)) if !id.trim().is_empty() => Some(id.clone()),
            Some(other) => {
                return Err(VerbError::BadArgs {
                    why: format!("parent must be an instance id string, got {other}"),
                });
            }
        };
        let requested = match args.get("model") {
            None | Some(Value::Null) => None,
            Some(Value::String(key)) if !key.trim().is_empty() => Some(key.clone()),
            Some(other) => {
                return Err(VerbError::BadArgs {
                    why: format!("model must be a catalog key string, got {other}"),
                });
            }
        };
        let model = match requested.or_else(|| self.default_model.clone()) {
            None => None,
            Some(key) => {
                let entry = self
                    .catalog
                    .get(&key)
                    .map_err(|why| VerbError::BadArgs { why })?;
                let config = GenerativeModelConfig {
                    model: entry.spec.clone(),
                    tools: tools::tool_specs(),
                    system_prompt: SYSTEM_PROMPT.to_string(),
                    backend_config: entry.backend.clone(),
                };
                let generator = (self.factory)(config).map_err(|why| VerbError::Failed { why })?;
                Some((key, generator))
            }
        };
        Ok(Box::new(Chat {
            agent: Principal::Agent(ctx.id.clone()),
            pool: self.pool.clone(),
            parent,
            model,
            transcript: Arc::new(Mutex::new(Transcript::default())),
            turn: None,
        }))
    }
}

// ---------------------------------------------------------------------------
// The instance
// ---------------------------------------------------------------------------

/// The shared transcript: verbs mutate it inside the cell, the turn task
/// streams into it from outside — the tty's pump pattern. The lock is never
/// held across an await.
#[derive(Default)]
struct Transcript {
    entries: Vec<Entry>,
    /// Index of the assistant entry the running turn streams into.
    pending: Option<usize>,
    /// Standing subscriptions: refreshed by the turn task where watermarks
    /// moved, edited by the watch/unwatch verbs. Living beside the entries
    /// under one lock keeps cursor advance and splice atomic.
    subs: Vec<Subscription>,
}

/// One standing subscription: (instance, cursored read, args, budget) plus
/// where we are — the cursor the verb returned and the watermark at the
/// last look (the cheap "anything new?" check).
#[derive(Debug, Clone, serde::Serialize)]
struct Subscription {
    instance: String,
    verb: String,
    args: Value,
    #[serde(skip)]
    cursor: Option<Value>,
    #[serde(skip)]
    mark: u64,
}

impl Transcript {
    fn push(&mut self, author: Principal, body: Body) -> Entry {
        let entry = Entry {
            seq: self.entries.len() as u64,
            at: Utc::now(),
            author,
            body,
        };
        self.entries.push(entry.clone());
        entry
    }

    /// Open the streaming assistant entry the turn writes into.
    fn open_assistant(&mut self, author: Principal, model: String) {
        self.push(
            author,
            Body::Assistant {
                model,
                content: Vec::new(),
                tool_uses: Vec::new(),
                turn_end: None,
            },
        );
        self.pending = Some(self.entries.len() - 1);
    }

    /// Best-effort live view of the stream; [`Self::finalize`] replaces it
    /// with the canonical accumulation, so approximation here cannot drift.
    fn apply_part(&mut self, part: &MessagePart) {
        let Some(i) = self.pending else { return };
        let Body::Assistant {
            content, tool_uses, ..
        } = &mut self.entries[i].body
        else {
            return;
        };
        match part {
            MessagePart::ContentStart(start) => {
                let (index, block) = match start {
                    myco_models::ContentStart::Text { index } => (
                        *index,
                        Content::Text {
                            text: String::new(),
                        },
                    ),
                    myco_models::ContentStart::Image { index } => (
                        *index,
                        Content::Image {
                            source: String::new(),
                        },
                    ),
                    myco_models::ContentStart::Thinking {
                        index,
                        signature,
                        redacted,
                    } => (
                        *index,
                        Content::Thinking {
                            text: String::new(),
                            signature: signature.clone(),
                            redacted: *redacted,
                        },
                    ),
                };
                while content.len() <= index {
                    content.push(Content::Text {
                        text: String::new(),
                    });
                }
                content[index] = block;
            }
            MessagePart::ContentDelta(delta) => {
                let (index, chunk) = match delta {
                    myco_models::ContentDelta::Text { index, delta }
                    | myco_models::ContentDelta::Image { index, delta }
                    | myco_models::ContentDelta::Thinking { index, delta } => (*index, delta),
                };
                match content.get_mut(index) {
                    Some(Content::Text { text: t }) | Some(Content::Thinking { text: t, .. }) => {
                        t.push_str(chunk)
                    }
                    Some(Content::Image { source }) => source.push_str(chunk),
                    None => {}
                }
            }
            MessagePart::ToolUseStart(start) => {
                while tool_uses.len() <= start.index {
                    tool_uses.push(ToolUse {
                        id: String::new(),
                        name: String::new(),
                        input: json!({}),
                    });
                }
                tool_uses[start.index] = ToolUse {
                    id: start.id.clone(),
                    name: start.name.clone(),
                    input: json!({}),
                };
            }
            _ => {}
        }
    }

    /// Replace the streamed approximation with the canonical accumulation.
    fn finalize(&mut self, output: GenerateOutput) {
        let Some(i) = self.pending.take() else { return };
        if let Body::Assistant {
            content,
            tool_uses,
            turn_end,
            ..
        } = &mut self.entries[i].body
        {
            *content = output.content;
            *tool_uses = output.tool_uses;
            *turn_end = Some(output.turn_end_reason);
        }
    }

    /// End the pending entry without an output: cancel, interject, error.
    fn fail_pending(&mut self, reason: &str) {
        let Some(i) = self.pending.take() else { return };
        if let Body::Assistant { turn_end, .. } = &mut self.entries[i].body {
            *turn_end = Some(TurnEndReason::Other(reason.to_string()));
        }
    }
}

struct Chat {
    /// The chat speaking for itself on the bus: `Agent(instance id)` — the
    /// doctrine's naming, and the transcript's author for model entries.
    agent: Principal,
    pool: Pool,
    parent: Option<String>,
    model: Option<(String, Arc<dyn GenerativeModel>)>,
    transcript: Arc<Mutex<Transcript>>,
    turn: Option<tokio::task::JoinHandle<()>>,
}

impl Chat {
    fn turn_running(&self) -> bool {
        self.turn.as_ref().is_some_and(|t| !t.is_finished())
    }

    fn abort_turn(&mut self, reason: &str, signals: &Signals) -> bool {
        let Some(handle) = self.turn.take() else {
            return false;
        };
        let was_running = !handle.is_finished();
        handle.abort();
        let mut t = self.transcript.lock().unwrap_or_else(|e| e.into_inner());
        t.fail_pending(reason);
        drop(t);
        if was_running {
            signals.emit("turn_ended", json!({ "reason": reason }));
            signals.bump();
        }
        was_running
    }

    fn start_turn(&mut self, signals: &Signals) {
        let Some((model, generator)) = &self.model else {
            return;
        };
        self.turn = Some(tokio::spawn(run_turn(TurnCtx {
            transcript: Arc::clone(&self.transcript),
            generator: Arc::clone(generator),
            pool: self.pool.clone(),
            agent: self.agent.clone(),
            model: model.clone(),
            signals: signals.clone(),
        })));
    }
}

/// Everything one turn needs, handed to the side-feed task.
struct TurnCtx {
    transcript: Arc<Mutex<Transcript>>,
    generator: Arc<dyn GenerativeModel>,
    pool: Pool,
    agent: Principal,
    model: String,
    signals: Signals,
}

/// One turn, as a side-feed: project the transcript, open the streaming
/// entry, drain the provider stream into it, finalize — and when the model
/// ends on `ToolUse`, dispatch the calls as the agent principal, append
/// the results, and generate again. The canonical accumulator runs with a
/// hook, so the live view and the finished entry can never disagree about
/// what the model said.
///
/// The task calls the pool freely (it is not a verb handler) but never
/// calls a verb on its own chat instance's mailbox — its writes go through
/// the shared transcript, and its self-knowledge through `sys.meta`, which
/// never enters a mailbox.
async fn run_turn(ctx: TurnCtx) {
    let TurnCtx {
        transcript,
        generator,
        pool,
        agent,
        model,
        signals,
    } = ctx;
    let chat_id = match &agent {
        Principal::Agent(id) => id.clone(),
        _ => unreachable!("the chat speaks as an agent"),
    };
    // Tool instances land beside the chat: same project, visible in the
    // same slice of the tree.
    let project = pool
        .call(&agent, &chat_id, "sys.meta", Value::Null)
        .await
        .ok()
        .and_then(|meta| meta.get("project").and_then(Value::as_str).map(String::from))
        .unwrap_or_default();

    signals.emit("turn_started", Value::Null);
    signals.bump();

    let reason = loop {
        refresh_subscriptions(&pool, &agent, &transcript, &signals).await;
        let messages = {
            let t = transcript.lock().unwrap_or_else(|e| e.into_inner());
            project::project(&t.entries)
        };
        {
            let mut t = transcript.lock().unwrap_or_else(|e| e.into_inner());
            t.open_assistant(agent.clone(), model.clone());
        }
        signals.bump();

        let stream = generator.generate(&messages);
        let hook_transcript = Arc::clone(&transcript);
        let hook_signals = signals.clone();
        let result = GenerateOutput::from_stream_with_hook(stream, move |part| {
            hook_transcript
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .apply_part(part);
            hook_signals.bump();
        })
        .await;

        match result {
            Ok(output) => {
                let tool_uses = output.tool_uses.clone();
                let end = output.turn_end_reason.clone();
                transcript
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .finalize(output);
                signals.bump();
                if end == TurnEndReason::ToolUse && !tool_uses.is_empty() {
                    let mut results = Vec::with_capacity(tool_uses.len());
                    for tool in &tool_uses {
                        results.push(tools::dispatch(&pool, &agent, &project, &model, tool).await);
                    }
                    {
                        let mut t = transcript.lock().unwrap_or_else(|e| e.into_inner());
                        t.push(agent.clone(), Body::ToolResults { results });
                    }
                    signals.bump();
                    continue;
                }
                break turn_end_name(&end);
            }
            Err(e) => {
                transcript
                    .lock()
                    .unwrap_or_else(|e2| e2.into_inner())
                    .fail_pending(&format!("error: {e}"));
                break "error".to_string();
            }
        }
    };
    signals.emit("turn_ended", json!({ "reason": reason }));
    signals.bump();
}

/// Chat-side cap on one splice, kind-agnostic: whatever the verb returned,
/// at most this much lands in the transcript (freshest end kept).
const SPLICE_BUDGET_BYTES: usize = 16 * 1024;

/// Refresh every standing subscription whose target's watermark moved:
/// call the cursored verb from the stored cursor, append the delta as a
/// watched entry, advance the cursor. Dead targets unsubscribe themselves
/// with a final note — a watch on a corpse is a leak, not a vigil.
async fn refresh_subscriptions(
    pool: &Pool,
    agent: &Principal,
    transcript: &Arc<Mutex<Transcript>>,
    signals: &Signals,
) {
    let subs: Vec<Subscription> = {
        let t = transcript.lock().unwrap_or_else(|e| e.into_inner());
        t.subs.clone()
    };
    for sub in subs {
        let moved = match pool.watermark(&sub.instance) {
            Ok(mark) if mark > sub.mark => Some(mark),
            Ok(_) => None,
            Err(_) => {
                // Target gone: drop the subscription, say so once.
                let mut t = transcript.lock().unwrap_or_else(|e| e.into_inner());
                t.subs.retain(|s| s.instance != sub.instance);
                t.push(
                    agent.clone(),
                    Body::Watched {
                        instance: sub.instance.clone(),
                        data: "[the watched instance is gone — subscription dropped]".into(),
                    },
                );
                signals.bump();
                None
            }
        };
        let Some(mark) = moved else { continue };
        let mut args = sub.args.clone();
        if let Some(cursor) = &sub.cursor
            && let Some(obj) = args.as_object_mut()
        {
            obj.insert("from".into(), cursor.clone());
        }
        let Ok(page) = pool.call(agent, &sub.instance, &sub.verb, args).await else {
            continue;
        };
        let next = page.get("next").cloned();
        let mut data = watched_text(&page);
        if data.len() > SPLICE_BUDGET_BYTES {
            let cut = (data.len() - SPLICE_BUDGET_BYTES..data.len())
                .find(|i| data.is_char_boundary(*i))
                .unwrap_or(0);
            data = format!("[truncated]\n{}", &data[cut..]);
        }
        let mut t = transcript.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(live) = t.subs.iter_mut().find(|s| s.instance == sub.instance) {
            live.mark = mark;
            if next.is_some() {
                live.cursor = next;
            }
        }
        if !data.trim().is_empty() {
            t.push(
                agent.clone(),
                Body::Watched {
                    instance: sub.instance.clone(),
                    data,
                },
            );
            drop(t);
            signals.bump();
        }
    }
}

/// The delta as text: cursored reads answer differently per kind, so take
/// the canonical fields we know ({data} from byte tails, {entries} from
/// chat tails via their plain text) and fall back to compact JSON.
fn watched_text(page: &Value) -> String {
    if let Some(data) = page.get("data").and_then(Value::as_str) {
        return data.to_string();
    }
    if let Some(entries) = page.get("entries").and_then(Value::as_array) {
        return entries
            .iter()
            .map(|e| {
                let author = e
                    .get("author")
                    .and_then(|a| a.get("id"))
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                let text = e.get("text").and_then(Value::as_str).unwrap_or_default();
                format!("{author}: {text}")
            })
            .collect::<Vec<_>>()
            .join("\n");
    }
    page.to_string()
}

fn turn_end_name(reason: &TurnEndReason) -> String {
    match reason {
        TurnEndReason::EndTurn => "end_turn".into(),
        TurnEndReason::MaxTokens => "max_tokens".into(),
        TurnEndReason::ToolUse => "tool_use".into(),
        TurnEndReason::Other(s) => s.clone(),
    }
}

impl Drop for Chat {
    /// The provider stream does not die with the instance, so the turn task
    /// must (the side-feed rule).
    fn drop(&mut self) {
        if let Some(turn) = &self.turn {
            turn.abort();
        }
    }
}

#[async_trait::async_trait]
impl Instance for Chat {
    async fn verb(
        &mut self,
        caller: &Principal,
        verb: &str,
        args: Value,
        signals: &Signals,
    ) -> Result<Value, VerbError> {
        match verb {
            "post" => {
                let text = args
                    .get("text")
                    .and_then(Value::as_str)
                    .map(str::trim_end)
                    .filter(|t| !t.is_empty())
                    .ok_or_else(|| VerbError::BadArgs {
                        why: "post needs a non-empty {text}".into(),
                    })?;
                let entry = {
                    let mut t = self.transcript.lock().unwrap_or_else(|e| e.into_inner());
                    t.push(
                        caller.clone(),
                        Body::Message {
                            text: text.to_string(),
                        },
                    )
                };
                signals.bump();
                // Mentions ride the pinned attention envelope. The entry
                // is the re-readable moment behind the event (tail from
                // seq), which is what makes the lossy feed honest.
                for name in mentions(text) {
                    let excerpt: String = text.chars().take(120).collect();
                    signals.emit(
                        myco_instance::events::ATTENTION,
                        myco_instance::events::Attention {
                            for_: vec![Principal::Human(name)],
                            title: format!("mentioned by {caller}"),
                            body: excerpt,
                            seq: Some(entry.seq),
                        }
                        .data(),
                    );
                }
                // Humans and *other* agents (a parent tasking this chat as
                // a subagent) start or interrupt turns. The chat never
                // answers itself — its own posts and entries are not
                // prompts — and never answers the system.
                let triggers = match caller {
                    Principal::Human(_) => true,
                    Principal::Agent(_) => *caller != self.agent,
                    Principal::System(_) => false,
                };
                if triggers && self.model.is_some() {
                    if self.turn_running() {
                        self.abort_turn("interrupted", signals);
                    }
                    self.start_turn(signals);
                }
                Ok(serde_json::to_value(entry).expect("entry serializes"))
            }
            "cancel" => {
                let cancelled = self.abort_turn("cancelled", signals);
                Ok(json!({ "cancelled": cancelled }))
            }
            "tail" => {
                let t = self.transcript.lock().unwrap_or_else(|e| e.into_inner());
                let len = t.entries.len() as u64;
                let from = args
                    .get("from")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .min(len);
                let max = args
                    .get("max_entries")
                    .and_then(Value::as_u64)
                    .unwrap_or(DEFAULT_TAIL);
                let upto = len.min(from.saturating_add(max));
                let slice = &t.entries[from as usize..upto as usize];
                Ok(json!({ "entries": slice, "next": upto, "len": len }))
            }
            "text" => {
                let t = self.transcript.lock().unwrap_or_else(|e| e.into_inner());
                let mut out = String::new();
                for e in &t.entries {
                    out.push_str(&format!("{}: {}\n", e.author, e.body.plain()));
                }
                Ok(json!({ "text": out, "len": t.entries.len() as u64 }))
            }
            "watch" => {
                let target = args
                    .get("instance")
                    .and_then(Value::as_str)
                    .filter(|t| !t.trim().is_empty())
                    .ok_or_else(|| VerbError::BadArgs {
                        why: "watch needs {instance}".into(),
                    })?;
                if matches!(&self.agent, Principal::Agent(own) if own == target) {
                    return Err(VerbError::BadArgs {
                        why: "a chat cannot watch itself".into(),
                    });
                }
                // sys.spec never enters a mailbox, and the priming read is
                // on another instance that cannot call back — both safe
                // from a verb handler.
                let spec = self.pool.call(caller, target, "sys.spec", Value::Null).await?;
                let empty = Vec::new();
                let verbs = spec["verbs"].as_array().unwrap_or(&empty);
                let cursored =
                    |v: &&Value| v["cursored"] == json!(true);
                let verb = match args.get("verb").and_then(Value::as_str) {
                    Some(name) => {
                        let found = verbs.iter().find(|v| v["name"] == json!(name));
                        match found {
                            Some(v) if cursored(&v) => name.to_string(),
                            Some(_) => {
                                return Err(VerbError::BadArgs {
                                    why: format!(
                                        "{name} is not cursored — a watch needs a delta read"
                                    ),
                                });
                            }
                            None => {
                                return Err(VerbError::BadArgs {
                                    why: format!("the target has no verb {name:?}"),
                                });
                            }
                        }
                    }
                    None => verbs
                        .iter()
                        .find(cursored)
                        .and_then(|v| v["name"].as_str())
                        .map(String::from)
                        .ok_or_else(|| VerbError::BadArgs {
                            why: "the target has no cursored verb to watch".into(),
                        })?,
                };
                let sub_args = match args.get("args") {
                    None | Some(Value::Null) => json!({}),
                    Some(Value::Object(o)) => Value::Object(o.clone()),
                    Some(other) => {
                        return Err(VerbError::BadArgs {
                            why: format!("args must be an object, got {other}"),
                        });
                    }
                };
                // Watching starts from now: prime the cursor by asking for
                // the position past everything current (cursored reads
                // clamp to their end).
                let mut prime = sub_args.clone();
                if let Some(obj) = prime.as_object_mut() {
                    obj.insert("from".into(), json!(u64::MAX));
                }
                let cursor = self
                    .pool
                    .call(caller, target, &verb, prime)
                    .await
                    .ok()
                    .and_then(|page| page.get("next").cloned());
                let mark = self.pool.watermark(target).unwrap_or(0);
                {
                    let mut t = self.transcript.lock().unwrap_or_else(|e| e.into_inner());
                    t.subs.retain(|sub| sub.instance != target);
                    t.subs.push(Subscription {
                        instance: target.to_string(),
                        verb: verb.clone(),
                        args: sub_args,
                        cursor,
                        mark,
                    });
                }
                signals.bump();
                Ok(json!({"watching": target, "verb": verb}))
            }
            "unwatch" => {
                let target = args
                    .get("instance")
                    .and_then(Value::as_str)
                    .ok_or_else(|| VerbError::BadArgs {
                        why: "unwatch needs {instance}".into(),
                    })?;
                let dropped = {
                    let mut t = self.transcript.lock().unwrap_or_else(|e| e.into_inner());
                    let before = t.subs.len();
                    t.subs.retain(|sub| sub.instance != target);
                    before != t.subs.len()
                };
                signals.bump();
                Ok(json!({"dropped": dropped}))
            }
            "about" => {
                let (len, watching) = {
                    let t = self.transcript.lock().unwrap_or_else(|e| e.into_inner());
                    (
                        t.entries.len() as u64,
                        serde_json::to_value(&t.subs).expect("subs serialize"),
                    )
                };
                Ok(json!({
                    "parent": self.parent,
                    "model": self.model.as_ref().map(|(key, _)| key.clone()),
                    "len": len,
                    "turn_running": self.turn_running(),
                    "watching": watching,
                }))
            }
            other => Err(VerbError::UnknownVerb { verb: other.into() }),
        }
    }
}

/// `@name` tokens in a post: ascii word characters after an `@`, lowered
/// to match user-id normalization. Distinct, in order of appearance.
fn mentions(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '@' {
            continue;
        }
        let mut name = String::new();
        while let Some(&n) = chars.peek() {
            if n.is_ascii_alphanumeric() || n == '_' || n == '-' {
                name.push(n.to_ascii_lowercase());
                chars.next();
            } else {
                break;
            }
        }
        if !name.is_empty() && !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

#[cfg(test)]
mod tests;

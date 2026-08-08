//! The `chat` kind: a conversation as an instance.
//!
//! v2's "session" dissolves here — a chat is an ordinary instance in the
//! pool, created by anyone, watched like anything else. A **subagent** is
//! nothing but a chat created under another chat; parentage is L1 identity
//! ([`InstanceInfo::parent`](myco_instance::InstanceInfo)), fixed at birth
//! and readable from the listing, so there is no child machinery here and
//! no parent field for this kind to keep in sync. The transcript is
//! shared, attributed, and multiplayer: `post` appends, `tail` cursors,
//! and the author is always the authenticated [`Principal`] the bus
//! resolved — never anything the arguments claim.
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

use std::sync::Arc;

use chrono::{DateTime, Utc};
use myco_instance::{Instance, Kind, KindSpec, Principal, Shared, VerbError, VerbSpec};
use myco_models::{
    Content, GenerateOutput, GenerativeModel, GenerativeModelConfig, MessagePart, ModelCatalog,
    ToolUse, TurnEndReason,
};
use myco_runtime::Signals;
use serde_json::{Value, json};

mod project;
pub use project::project;

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
        VerbSpec::read("about", "the chat's shape: {model, len, turn_running}"),
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
}

impl Body {
    fn plain(&self) -> String {
        match self {
            Body::Message { text } => text.clone(),
            Body::Assistant { content, .. } => myco_models::content_text(content),
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
    catalog: Arc<ModelCatalog>,
    /// Applied when create args omit `model`. `None` + empty catalog is the
    /// modelless workspace: chats are pure transcripts.
    default_model: Option<String>,
    factory: ModelFactory,
}

impl Default for ChatKind {
    fn default() -> Self {
        Self::new(ModelCatalog::default(), None)
    }
}

impl ChatKind {
    pub fn new(catalog: ModelCatalog, default_model: Option<String>) -> Self {
        Self::with_factory(
            catalog,
            default_model,
            Arc::new(|config| myco_models::new(config).map_err(|e| e.to_string())),
        )
    }

    /// The test seam: a factory that answers with a scripted model.
    pub fn with_factory(
        catalog: ModelCatalog,
        default_model: Option<String>,
        factory: ModelFactory,
    ) -> Self {
        Self {
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

    fn create(&self, args: Value, signals: Signals) -> Result<Box<dyn Instance>, VerbError> {
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
                    tools: Vec::new(),
                    system_prompt: SYSTEM_PROMPT.to_string(),
                    backend_config: entry.backend.clone(),
                };
                let generator = (self.factory)(config).map_err(|why| VerbError::Failed { why })?;
                Some((key, generator))
            }
        };
        Ok(Box::new(Chat {
            model,
            transcript: Shared::new(Transcript::default(), signals),
            turn: None,
        }))
    }
}

// ---------------------------------------------------------------------------
// The instance
// ---------------------------------------------------------------------------

/// The transcript: verbs mutate it inside the cell, the turn task streams
/// into it from outside. It is held in a [`Shared`] — the framework's
/// spelling of that arrangement, so the bump that wakes watchers cannot be
/// forgotten and the lock cannot be held across an await.
#[derive(Default)]
struct Transcript {
    entries: Vec<Entry>,
    /// Index of the assistant entry the running turn streams into.
    pending: Option<usize>,
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

    /// Open the streaming assistant entry the turn writes into. The author
    /// is `Agent(model key)` for now; it becomes `Agent(chat id)` — the
    /// doctrine's naming — when the tool dispatcher gives the kind its own
    /// instance id.
    fn open_assistant(&mut self, model: String) {
        self.push(
            Principal::Agent(model.clone()),
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
    model: Option<(String, Arc<dyn GenerativeModel>)>,
    transcript: Shared<Transcript>,
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
        if handle.is_finished() {
            // The task already closed its own entry; there is nothing left
            // to fail and nobody to wake.
            return false;
        }
        handle.abort();
        self.transcript.with(|t| t.fail_pending(reason));
        signals.emit("turn_ended", json!({ "reason": reason }));
        true
    }

    fn start_turn(&mut self) {
        let Some((model, generator)) = &self.model else {
            return;
        };
        self.turn = Some(tokio::spawn(run_turn(
            self.transcript.clone(),
            Arc::clone(generator),
            model.clone(),
        )));
    }
}

/// One turn, as a side-feed: project the transcript, open the streaming
/// entry, drain the provider stream into it, finalize. Uses the canonical
/// accumulator with a hook, so the live view and the final entry can never
/// disagree about what the model said. Every write goes through the
/// [`Shared`], so every delta wakes watchers without a single hand-written
/// bump.
async fn run_turn(
    transcript: Shared<Transcript>,
    generator: Arc<dyn GenerativeModel>,
    model: String,
) {
    let messages = transcript.read(|t| project(&t.entries));
    transcript.with(|t| t.open_assistant(model.clone()));
    transcript.signals().emit("turn_started", Value::Null);

    let stream = generator.generate(&messages);
    let hook = transcript.clone();
    let result =
        GenerateOutput::from_stream_with_hook(stream, move |part| hook.with(|t| t.apply_part(part)))
            .await;

    let reason = match result {
        Ok(output) => {
            let reason = turn_end_name(&output.turn_end_reason);
            transcript.with(|t| t.finalize(output));
            reason
        }
        Err(e) => {
            transcript.with(|t| t.fail_pending(&format!("error: {e}")));
            "error".to_string()
        }
    };
    transcript
        .signals()
        .emit("turn_ended", json!({ "reason": reason }));
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
                let entry = self.transcript.with(|t| {
                    t.push(
                        caller.clone(),
                        Body::Message {
                            text: text.to_string(),
                        },
                    )
                });
                // Only a person starts (or interrupts) a turn: the model
                // answers humans, not itself and not the system.
                if matches!(caller, Principal::Human(_)) && self.model.is_some() {
                    if self.turn_running() {
                        self.abort_turn("interrupted", signals);
                    }
                    self.start_turn();
                }
                Ok(serde_json::to_value(entry).expect("entry serializes"))
            }
            "cancel" => {
                let cancelled = self.abort_turn("cancelled", signals);
                Ok(json!({ "cancelled": cancelled }))
            }
            "tail" => Ok(self.transcript.read(|t| {
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
                json!({ "entries": slice, "next": upto, "len": len })
            })),
            "text" => Ok(self.transcript.read(|t| {
                let mut out = String::new();
                for e in &t.entries {
                    out.push_str(&format!("{}: {}\n", e.author, e.body.plain()));
                }
                json!({ "text": out, "len": t.entries.len() as u64 })
            })),
            "about" => {
                let len = self.transcript.read(|t| t.entries.len() as u64);
                Ok(json!({
                    "model": self.model.as_ref().map(|(key, _)| key.clone()),
                    "len": len,
                    "turn_running": self.turn_running(),
                }))
            }
            other => Err(VerbError::UnknownVerb { verb: other.into() }),
        }
    }
}

#[cfg(test)]
mod tests;

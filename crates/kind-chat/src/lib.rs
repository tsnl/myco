//! The `chat` kind: a conversation as an instance.
//!
//! v2's "session" dissolves here — a chat is an ordinary instance in the
//! pool, created by anyone, watched like anything else. A **subagent** is
//! nothing but a chat created under another chat; parentage is L1 identity
//! ([`InstanceInfo::parent`](myco_instance::InstanceInfo)), fixed at birth
//! and readable from the listing, so there is no child machinery here and
//! no parent field for this kind to keep in sync. At this stack position
//! the kind is a shared,
//! attributed transcript with no model attached: `post` appends, `tail`
//! cursors, and multiple principals converse. The turn engine (a model
//! answering posts) arrives two PRs up; its entries will be new [`Body`]
//! variants in the same log.
//!
//! Attribution is a framework fact: `post` records the authenticated
//! [`Principal`] the bus resolved, never anything the arguments claim.
//! `post` is deliberately an *open* write — a conversation is multiplayer,
//! so exclusivity (the driver seat) governs nothing here yet; the seat
//! becomes meaningful when a model holds it during a turn.

use chrono::{DateTime, Utc};
use myco_instance::{Instance, Kind, KindSpec, Principal, VerbError, VerbSpec};
use myco_runtime::Signals;
use serde_json::{Value, json};

static CHAT_SPEC: KindSpec = KindSpec {
    kind: "chat",
    version: 1,
    doc: "a conversation: an attributed, append-only transcript shared by humans and agents",
    verbs: &[
        VerbSpec::write("post", "append a message {text}; the author is the caller, always"),
        VerbSpec::cursored_read(
            "tail",
            "entries from sequence {from}, at most {max_entries} (default 200); returns {next} \
             and the total {len}",
        ),
        VerbSpec::read(
            "text",
            "the transcript as plain text, one `author: message` line per entry",
        ),
        VerbSpec::read("about", "the chat's shape: {len}"),
    ],
    primary_render: "tail",
    recommended_context: "text",
};

const DEFAULT_TAIL: u64 = 200;

/// One transcript entry. `seq` is dense from 0, so `tail`'s cursor is also
/// an entry count; `body` is tagged for the variants later PRs add
/// (assistant turns, tool records) — readers match on `t` and skip what
/// they don't know.
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
    Message { text: String },
}

impl Body {
    fn plain(&self) -> &str {
        match self {
            Body::Message { text } => text,
        }
    }
}

pub struct ChatKind;

impl Kind for ChatKind {
    fn spec(&self) -> &'static KindSpec {
        &CHAT_SPEC
    }

    fn create(&self, _args: Value, _signals: Signals) -> Result<Box<dyn Instance>, VerbError> {
        Ok(Box::new(Chat {
            entries: Vec::new(),
        }))
    }
}

/// The canonical state: the transcript, and nothing else. What a chat *is*
/// — who made it, what it hangs under — is L1's to know; a kind that also
/// kept a copy would be a second answer to the same question.
struct Chat {
    entries: Vec<Entry>,
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
                let entry = Entry {
                    seq: self.entries.len() as u64,
                    at: Utc::now(),
                    author: caller.clone(),
                    body: Body::Message {
                        text: text.to_string(),
                    },
                };
                self.entries.push(entry.clone());
                signals.bump();
                Ok(serde_json::to_value(entry).expect("entry serializes"))
            }
            "tail" => {
                let len = self.entries.len() as u64;
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
                let slice = &self.entries[from as usize..upto as usize];
                Ok(json!({
                    "entries": slice,
                    "next": upto,
                    "len": len,
                }))
            }
            "text" => {
                let mut out = String::new();
                for e in &self.entries {
                    out.push_str(&format!("{}: {}\n", e.author, e.body.plain()));
                }
                Ok(json!({ "text": out, "len": self.entries.len() as u64 }))
            }
            "about" => Ok(json!({
                "len": self.entries.len() as u64,
            })),
            other => Err(VerbError::UnknownVerb { verb: other.into() }),
        }
    }
}

#[cfg(test)]
mod tests;

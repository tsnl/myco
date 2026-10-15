use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct OperationId {
    pub run: String,
    pub sequence: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Principal {
    Human(String),
    Agent(String),
    System(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Content {
    Text(String),
    Blob { digest: String, media_type: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Call {
    pub tool: String,
    pub arguments: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Reply {
    pub content: Vec<Content>,
    pub calls: Vec<Call>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Outcome {
    Succeeded(Vec<Content>),
    Failed(String),
    Cancelled,
    /// The adapter cannot establish whether the external effect happened.
    Unknown(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextRef {
    pub lineage: String,
    pub revision: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Message {
    Input {
        author: Principal,
        content: Vec<Content>,
        caused_by: Option<OperationId>,
    },
    Assistant {
        generation: OperationId,
        reply: Reply,
    },
    Tool {
        generation: OperationId,
        index: usize,
        outcome: Outcome,
    },
    Summary {
        source: ContextRef,
        producer: OperationId,
        content: Vec<Content>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Context {
    pub at: ContextRef,
    pub derived_from: Option<ContextRef>,
    pub messages: Arc<Vec<Message>>,
}

impl Context {
    pub fn new(lineage: &str, messages: Vec<Message>) -> Self {
        Self {
            at: ContextRef {
                lineage: lineage.into(),
                revision: 0,
            },
            derived_from: None,
            messages: Arc::new(messages),
        }
    }

    pub fn append(&self, messages: impl IntoIterator<Item = Message>) -> Self {
        let mut next = self.clone();
        next.at.revision += 1;
        Arc::make_mut(&mut next.messages).extend(messages);
        next
    }

    pub fn derive(&self, lineage: &str, messages: Vec<Message>) -> Self {
        Self {
            derived_from: Some(self.at.clone()),
            ..Self::new(lineage, messages)
        }
    }

    /// A generation boundary has no unmatched or duplicate tool observations.
    pub fn validate_ready(&self) -> Result<(), &'static str> {
        let mut pending: Option<(&OperationId, Vec<bool>)> = None;
        let mut seen = std::collections::BTreeSet::new();
        for message in self.messages.iter() {
            match message {
                Message::Tool {
                    generation, index, ..
                } => {
                    let (expected, slots) = pending.as_mut().ok_or("orphan tool observation")?;
                    if generation != *expected || *index >= slots.len() || slots[*index] {
                        return Err("tool observation does not match a pending call");
                    }
                    slots[*index] = true;
                    if slots.iter().all(|done| *done) {
                        pending = None;
                    }
                }
                _ if pending.is_some() => {
                    return Err("message interrupts an unfinished tool round");
                }
                Message::Assistant { generation, reply } => {
                    if !seen.insert(generation) {
                        return Err("duplicate generation in context");
                    }
                    if !reply.calls.is_empty() {
                        pending = Some((generation, vec![false; reply.calls.len()]));
                    }
                }
                _ => {}
            }
        }
        if pending.is_some() {
            return Err("context has unresolved tool calls");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Document {
    pub name: String,
    pub revision: String,
    pub content: Vec<Content>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub schema: Value,
    /// An immutable interpreter-side binding; never supplied by model arguments.
    pub binding: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    pub model: String,
    pub parameters: Value,
    pub documents: Arc<Vec<Document>>,
    pub tools: Arc<Vec<Tool>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelInput {
    pub context: Context,
    pub settings: Settings,
}

/// Evidence returned by serving, not reconstructed from decoded text.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Tokens {
    pub policy_revision: String,
    pub tokenizer: String,
    pub template: String,
    pub logprob_convention: String,
    pub prompt: Vec<u32>,
    pub completion: Vec<u32>,
    pub logprobs: Vec<f64>,
}

impl Tokens {
    pub fn validate(&self) -> Result<(), &'static str> {
        if [
            &self.policy_revision,
            &self.tokenizer,
            &self.template,
            &self.logprob_convention,
        ]
        .iter()
        .any(|s| s.is_empty())
        {
            return Err("token evidence has incomplete provenance");
        }
        if self.completion.is_empty()
            || self.completion.len() != self.logprobs.len()
            || self.logprobs.iter().any(|v| !v.is_finite() || *v > 0.0)
        {
            return Err("token evidence has invalid completion probabilities");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Completion {
    pub reply: Reply,
    pub finish: Finish,
    pub tokens: Option<Tokens>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Finish {
    Complete,
    Limit,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Failure {
    pub reason: String,
    pub partial: Vec<Content>,
}

/// A capture boundary usable by arbitrary controllers, including existing agents.
/// An open attempt has no result; a superseded reply remains recorded but unapplied.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GenerationRecord {
    pub generation: OperationId,
    pub input: ModelInput,
    pub result: Option<Result<Completion, Failure>>,
    pub applied: bool,
}

impl GenerationRecord {
    pub fn candidate(&self) -> Option<Sample> {
        let Some(Ok(output)) = &self.result else {
            return None;
        };
        (self.applied && output.finish == Finish::Complete).then(|| Sample {
            generation: self.generation.clone(),
            input: self.input.clone(),
            output: output.clone(),
        })
    }
}

/// Candidate example. Selection, rewards, and SFT loss masks belong to exporters.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Sample {
    pub generation: OperationId,
    pub input: ModelInput,
    pub output: Completion,
}

impl Sample {
    /// This establishes token evidence, not suitability for a particular RL algorithm.
    pub fn token_evidence(&self) -> Result<&Tokens, &'static str> {
        let tokens = self
            .output
            .tokens
            .as_ref()
            .ok_or("serving did not supply token evidence")?;
        tokens.validate()?;
        Ok(tokens)
    }
}

#![doc = include_str!("README.md")]

use std::{
    collections::{HashMap, hash_map::Entry},
    ops::Index,
    slice::SliceIndex,
    sync::Arc,
};

use serde_json::{Map, Value};

//
// Thread
//

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Thread {
    turns: Vec<Turn>,
}

impl Thread {
    pub fn new(turns: Vec<Turn>) -> Self {
        Self { turns }
    }
    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }
    pub fn push(&mut self, turn: Turn) {
        self.turns.push(turn);
    }
    pub fn blob_refs(&self) -> impl Iterator<Item = BlobRef> + '_ {
        self.turns
            .iter()
            .flat_map(Turn::contents)
            .flat_map(Content::blob_refs)
    }
    pub fn validate_content(&self, store: &BlobStore) -> Result<(), ContentError> {
        for reference in self.blob_refs() {
            store.get(reference)?;
        }
        Ok(())
    }
}

impl<I: SliceIndex<[Turn]>> Index<I> for Thread {
    type Output = I::Output;

    fn index(&self, index: I) -> &Self::Output {
        &self.turns[index]
    }
}

//
// Turns
//

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub kind: TurnKind,
    pub provider_info: Map<String, Value>,
}

impl Turn {
    pub fn new(kind: TurnKind) -> Self {
        Self {
            kind,
            provider_info: Map::new(),
        }
    }
    fn contents(&self) -> impl Iterator<Item = &Content> {
        let (content, responses): (&Content, &[ToolUseResponse]) = match &self.kind {
            TurnKind::User(turn) => (&turn.content, &turn.tool_use_responses),
            TurnKind::Assistant(turn) => (&turn.content, &[]),
        };
        std::iter::once(content).chain(responses.iter().map(|response| &response.content))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnKind {
    User(UserTurn),
    Assistant(AssistantTurn),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserTurn {
    pub author: Author,
    pub content: Content,
    pub tool_use_responses: Vec<ToolUseResponse>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AssistantTurn {
    pub content: Content,
    pub tool_use_requests: Vec<ToolUseRequest>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Author {
    Human,
    System,
}

//
// Content
//

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Content {
    pub parts: Vec<ContentPart>,
}

impl Content {
    pub fn blob_refs(&self) -> impl Iterator<Item = BlobRef> + '_ {
        self.parts.iter().filter_map(|part| match part {
            ContentPart::Image { blob } => Some(*blob),
            _ => None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentPart {
    Text { content: String },
    Image { blob: BlobRef },
    Reasoning(ReasoningContentPart),
    Refusal(RefusalContentPart),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReasoningContentPart {
    Text {
        text: String,
        signature: Option<String>,
    },
    Encrypted {
        id: String,
        summary: Vec<String>,
        data: String,
    },
    Redacted(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusalContentPart {
    pub kind: Option<String>,
    pub message: String,
}

//
// Blob store
//

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlobRef(pub uuid::Uuid);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Blob {
    pub media_type: String,
    pub data: Arc<[u8]>,
}

#[derive(Debug, Clone, Default)]
pub struct BlobStore {
    blobs: HashMap<BlobRef, Blob>,
}

impl BlobStore {
    pub fn insert(&mut self, reference: BlobRef, blob: Blob) -> Result<(), ContentError> {
        match self.blobs.entry(reference) {
            Entry::Vacant(entry) => {
                entry.insert(blob);
                Ok(())
            }
            Entry::Occupied(_) => Err(ContentError::AlreadyExists(reference)),
        }
    }
    pub fn get(&self, reference: BlobRef) -> Result<&Blob, ContentError> {
        self.blobs
            .get(&reference)
            .ok_or(ContentError::Missing(reference))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ContentError {
    #[error("missing content blob {0:?}")]
    Missing(BlobRef),
    #[error("content blob {0:?} is already registered")]
    AlreadyExists(BlobRef),
}

//
// Tools
//

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ToolCallId(pub uuid::Uuid);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolUseRequest {
    pub id: ToolCallId,
    pub name: String,
    pub arguments: Result<Value, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolUseResponse {
    pub id: ToolCallId,
    pub kind: ToolUseResponseKind,
    pub content: Content,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolUseResponseKind {
    Error,
    Success,
    Backgrounded,
    Unknown,
}

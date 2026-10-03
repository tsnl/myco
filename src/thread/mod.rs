#![doc = include_str!("README.md")]

use std::{ops::Index, slice::SliceIndex};

use serde_json::Value;

use crate::blob::{BlobError, BlobRef, BlobStore};

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
    pub fn validate_content(&self, store: &BlobStore) -> Result<(), BlobError> {
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
}

impl Turn {
    pub fn new(kind: TurnKind) -> Self {
        Self { kind }
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
    pub content: Content,
    pub tool_use_responses: Vec<ToolUseResponse>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AssistantTurn {
    pub content: Content,
    pub tool_use_requests: Vec<ToolUseRequest>,
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
        self.parts.iter().filter_map(|part| match &part.kind {
            ContentPartKind::Image { blob } => Some(*blob),
            _ => None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentPart {
    pub author: Author,
    pub kind: ContentPartKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Author {
    Human,
    Assistant,
    Tool,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentPartKind {
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
        provider_id: String,
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
// Tools
//

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ToolCallId(pub uuid::Uuid);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolUseRequest {
    pub id: ToolCallId,
    pub provider_id: Option<String>,
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

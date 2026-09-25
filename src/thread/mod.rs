#![doc = include_str!("README.md")]

use std::{ops::Index, slice::SliceIndex};

use serde_json::Value;

//
// Thread
//

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Thread {
    turns: Vec<TurnPair>,
}

impl Thread {
    pub fn new(turns: Vec<TurnPair>) -> Self {
        Self { turns }
    }
    pub fn turns(&self) -> &[TurnPair] {
        &self.turns
    }
    pub fn push(&mut self, turn: Turn) -> Result<(), TurnPushError> {
        let expected = self.next_turn_kind();
        let received = turn.kind();

        if received != expected {
            return Err(TurnPushError::BadKind { expected, received });
        }

        // TODO: finish this implementation
    }

    fn next_turn_kind(&self) -> TurnKind {
        if let Some(back) = self.turns.back() {
            back.next_turn_kind()
        } else {
            TurnKind::User
        }
    }
}

impl<I: SliceIndex<[TurnPair]>> Index<I> for Thread {
    type Output = I::Output;

    fn index(&self, index: I) -> &Self::Output {
        &self.turns[index]
    }
}

#[thiserror::Error]
pub enum TurnPushError {
    #[err("Expected turn kind {expected}, received turn kind {received}")]
    BadKind { expected: TurnKind, received: TurnKind }
}

//
// Turns
//

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnPair {
    user_turn: UserTurn,
    assistant_turn: Option<AssistantTurn>,
}
impl TurnPair {
    fn next_turn_kind(&self) -> TurnKind {
        match &self.assistant_turn {
            None => TurnKind::Assistant,
            Some(_) => TurnKind::User,
        }
    }
}

pub enum Turn {
    User(UserTurn),
    Assistant(AssistantTurn),
}
impl Turn {
    fn kind(&self) -> TurnKind {
        match self {
            User(_) => TurnKind::User,
            Assistant(_) => TurnKind::Assistant,
        }
    }
}

pub enum TurnKind {
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserTurn {
    author: Author,
    content: Content,
    tool_use_responses: Vec<ToolUseResponse>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssistantTurn {
    content: Content,
    tool_use_requests: Vec<ToolUseRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Author {
    Human,
    System,`
}

//
// Content
//

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Content {
    parts: Vec<ContentPart>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentPart {
    Text { content: String },
    Image { url: String },
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
    kind: Option<String>,
    message: String,
}

//
// Tools
//

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ToolCallId(pub uuid::Uuid);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolUseRequest {
    id: ToolCallId,
    provider_call_id: Option<String>,
    name: String,
    arguments: Result<Value, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolUseResponse {
    kind: ToolUseResponseKind,
    content: Content,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolUseResponseKind {
    Error,
    Success,
    Backgrounded,
}

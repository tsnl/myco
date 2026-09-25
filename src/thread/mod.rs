#![doc = include_str!("README.md")]

use std::{ops::Index, slice::SliceIndex};

use serde_json::Value;

//
// Thread
//

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Thread {
    entries: Vec<Entry>,
}

impl Thread {
    pub fn from_entries(entries: Vec<Entry>) -> Self {
        Self { entries }
    }
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }
    pub fn push(&mut self, entry: Entry) {
        self.entries.push(entry);
    }
}

impl<I: SliceIndex<[Entry]>> Index<I> for Thread {
    type Output = I::Output;

    fn index(&self, index: I) -> &Self::Output {
        &self.entries[index]
    }
}

//
// Identifiers
//

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ToolCallId(pub uuid::Uuid);

//
// Entries
//

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Turn(Turn),
    Warning(String),
    Error(String),
    Notification(String),
}

//
// Turns
//

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Turn {
    pub sender: Sender,
    pub content: Vec<ContentPart>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sender {
    Assistant,
    User,
    Tool,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentPart {
    Text {
        content: String,
    },
    Image {
        url: String,
    },
    Reasoning(Reasoning),
    Refusal(String),
    ToolCall {
        id: ToolCallId,
        provider_call_id: Option<String>,
        name: String,
        arguments: Result<Value, String>,
    },
    ToolResponse {
        id: ToolCallId,
        result: ToolResponseResult,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reasoning {
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
pub enum ToolResponseResult {
    Completed { result: String, is_error: bool },
    Backgrounded,
}

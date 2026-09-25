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
    User(String),
    Assistant {
        content: Vec<ContentPart>,
    },
    ToolResult {
        call_id: ToolCallId,
        output: String,
        is_error: bool,
    },
    System(String),
    Warning(String),
    Error(String),
    Notification(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentPart {
    Text(String),
    Reasoning {
        text: String,
        signature: Option<String>,
    },
    EncryptedReasoning {
        id: String,
        summary: Vec<String>,
        data: String,
    },
    RedactedReasoning(String),
    Refusal(String),
    ToolCall {
        id: ToolCallId,
        provider_call_id: Option<String>,
        name: String,
        arguments: Result<Value, String>,
    },
}

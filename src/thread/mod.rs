#![doc = include_str!("README.md")]

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
    pub fn fork(&self, prefix_len: usize) -> Option<Self> {
        let entries = self.entries.get(..prefix_len)?.to_vec();
        Some(Self::from_entries(entries))
    }
}

//
// Identifiers
//

pub type OperationId = uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InferenceRecordId(pub u128);

//
// Entries
//

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    User(String),
    Assistant {
        content: Vec<ContentPart>,
        inference_record: Option<InferenceRecordId>,
    },
    ToolResult {
        operation: OperationId,
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
    Reasoning(String),
    Refusal(String),
    ToolCall {
        operation: OperationId,
        name: String,
        arguments: Result<Value, String>,
    },
}

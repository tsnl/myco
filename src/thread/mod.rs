#![doc = include_str!("README.md")]

use serde_json::Value;

//
// Thread
//

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thread {
    id: ThreadId,
    entries: Vec<Entry>,
}

impl Thread {
    pub fn new(id: ThreadId) -> Self {
        Self::from_parts(id, vec![])
    }

    pub fn from_parts(id: ThreadId, entries: Vec<Entry>) -> Self {
        Self { id, entries }
    }

    pub fn id(&self) -> ThreadId {
        self.id
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn append(&mut self, entry: Entry) {
        self.entries.push(entry);
    }

    pub fn fork(&self, id: ThreadId, prefix_len: usize) -> Option<Self> {
        let entries = self.entries.get(..prefix_len)?.to_vec();
        Some(Self::from_parts(id, entries))
    }
}

//
// Identifiers
//

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ThreadId(pub u128);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OperationId(pub u128);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EvidenceId(pub u128);

//
// Entries
//

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    User(String),
    Assistant {
        content: Vec<ContentPart>,
        evidence: Option<EvidenceId>,
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

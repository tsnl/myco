#![doc = include_str!("README.md")]

use std::{
    future::Future,
    ops::Range,
    pin::Pin,
    sync::{Arc, Mutex},
};

use serde_json::Value;

mod memory;

//
// Threads
//

#[derive(Clone)]
pub struct Threads {
    store: Arc<dyn ThreadStore>,
}

impl Threads {
    pub fn new(store: Arc<dyn ThreadStore>) -> Self {
        Self { store }
    }

    pub async fn create(&self, request: CreateThread) -> Result<ThreadVersion, Error> {
        self.store.apply(Mutation::Create(request)).await
    }

    pub async fn read(&self, version: ThreadVersion) -> Result<Thread, Error> {
        self.store.read(version).await
    }

    pub async fn append(&self, request: AppendEntries) -> Result<ThreadVersion, Error> {
        self.store.apply(Mutation::Append(request)).await
    }

    pub async fn fork(&self, request: ForkThread) -> Result<ThreadVersion, Error> {
        self.store.apply(Mutation::Fork(request)).await
    }

    pub async fn operation(
        &self,
        operation: OperationId,
    ) -> Result<Option<OperationStatus>, Error> {
        self.store.operation(operation).await
    }

    pub async fn cancel(&self, operation: OperationId) -> Result<OperationStatus, Error> {
        self.store.cancel(operation).await
    }
}

//
// History
//

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ThreadId(pub u128);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OperationId(pub u128);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EvidenceId(pub u128);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Revision(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ThreadVersion {
    pub thread: ThreadId,
    pub revision: Revision,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryRef {
    pub version: ThreadVersion,
    pub entries: Range<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thread {
    version: ThreadVersion,
    entries: Vec<Entry>,
    sources: Vec<HistoryRef>,
}

impl Thread {
    pub fn from_parts(
        version: ThreadVersion,
        entries: Vec<Entry>,
        sources: Vec<HistoryRef>,
    ) -> Self {
        Self {
            version,
            entries,
            sources,
        }
    }

    pub fn version(&self) -> ThreadVersion {
        self.version
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn sources(&self) -> &[HistoryRef] {
        &self.sources
    }
}

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

//
// Mutations
//

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateThread {
    pub operation: OperationId,
    pub thread: ThreadId,
    pub entries: Vec<Entry>,
    pub sources: Vec<HistoryRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppendEntries {
    pub operation: OperationId,
    pub expected: ThreadVersion,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkThread {
    pub operation: OperationId,
    pub thread: ThreadId,
    pub source: ThreadVersion,
    pub prefix_len: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mutation {
    Create(CreateThread),
    Append(AppendEntries),
    Fork(ForkThread),
}

impl Mutation {
    pub fn operation(&self) -> OperationId {
        match self {
            Self::Create(request) => request.operation,
            Self::Append(request) => request.operation,
            Self::Fork(request) => request.operation,
        }
    }
}

//
// Storage
//

pub type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, Error>> + Send + 'a>>;

pub trait ThreadStore: Send + Sync {
    fn read(&self, version: ThreadVersion) -> StoreFuture<'_, Thread>;
    fn apply(&self, mutation: Mutation) -> StoreFuture<'_, ThreadVersion>;
    fn operation(&self, operation: OperationId) -> StoreFuture<'_, Option<OperationStatus>>;
    fn cancel(&self, operation: OperationId) -> StoreFuture<'_, OperationStatus>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperationStatus {
    Committed(ThreadVersion),
    Cancelled,
    Rejected(Error),
}

#[derive(Default)]
pub struct MemoryStore {
    state: Mutex<memory::State>,
}

//
// Errors
//

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("thread version {0:?} does not exist")]
    NotFound(ThreadVersion),
    #[error("thread {0:?} already exists")]
    AlreadyExists(ThreadId),
    #[error("expected {expected:?}, but the current version is {actual:?}")]
    Conflict {
        expected: ThreadVersion,
        actual: ThreadVersion,
    },
    #[error("operation {0:?} was cancelled")]
    Cancelled(OperationId),
    #[error("operation {0:?} was already used for a different mutation")]
    OperationConflict(OperationId),
    #[error("invalid history request: {0}")]
    InvalidRequest(String),
    #[error("thread store failed: {0}")]
    Store(String),
}

use std::{collections::HashMap, sync::MutexGuard};

use super::{
    AppendEntries, CreateThread, Entry, Error, ForkThread, HistoryRef, MemoryStore, Mutation,
    OperationId, OperationStatus, Revision, StoreFuture, Thread, ThreadId, ThreadStore,
    ThreadVersion,
};

//
// Store
//

#[derive(Default)]
pub(super) struct State {
    threads: HashMap<ThreadId, History>,
    operations: HashMap<OperationId, Record>,
}

struct History {
    current: Thread,
    lengths: Vec<usize>,
}

struct Record {
    mutation: Option<Mutation>,
    status: OperationStatus,
}

impl ThreadStore for MemoryStore {
    fn read(&self, version: ThreadVersion) -> StoreFuture<'_, Thread> {
        Box::pin(async move { self.lock()?.history(version)?.read(version) })
    }

    fn apply(&self, mutation: Mutation) -> StoreFuture<'_, ThreadVersion> {
        Box::pin(async move { self.lock()?.apply(mutation) })
    }

    fn operation(&self, operation: OperationId) -> StoreFuture<'_, Option<OperationStatus>> {
        Box::pin(async move {
            Ok(self
                .lock()?
                .operations
                .get(&operation)
                .map(|record| record.status.clone()))
        })
    }

    fn cancel(&self, operation: OperationId) -> StoreFuture<'_, OperationStatus> {
        Box::pin(async move { Ok(self.lock()?.cancel(operation)) })
    }
}

impl MemoryStore {
    fn lock(&self) -> Result<MutexGuard<'_, State>, Error> {
        self.state
            .lock()
            .map_err(|_| Error::Store("history lock is poisoned".into()))
    }
}

//
// History
//

impl State {
    fn history(&self, version: ThreadVersion) -> Result<&History, Error> {
        self.threads
            .get(&version.thread)
            .ok_or(Error::NotFound(version))
    }

    fn entries(&self, history: &HistoryRef) -> Result<&[Entry], Error> {
        self.history(history.version)?
            .entries(history.version)?
            .get(history.entries.clone())
            .ok_or_else(|| Error::InvalidRequest(format!("invalid history range {history:?}")))
    }

    fn create(&mut self, request: &CreateThread) -> Result<ThreadVersion, Error> {
        for source in &request.sources {
            self.entries(source)?;
        }
        self.insert(
            request.thread,
            request.entries.clone(),
            request.sources.clone(),
        )
    }

    fn insert(
        &mut self,
        id: ThreadId,
        entries: Vec<Entry>,
        sources: Vec<HistoryRef>,
    ) -> Result<ThreadVersion, Error> {
        if self.threads.contains_key(&id) {
            return Err(Error::AlreadyExists(id));
        }
        let version = ThreadVersion {
            thread: id,
            revision: Revision(0),
        };
        let lengths = vec![entries.len()];
        let current = Thread::from_parts(version, entries, sources);
        self.threads.insert(id, History { current, lengths });
        Ok(version)
    }

    fn append(&mut self, request: &AppendEntries) -> Result<ThreadVersion, Error> {
        self.threads
            .get_mut(&request.expected.thread)
            .ok_or(Error::NotFound(request.expected))?
            .append(request)
    }

    fn fork(&mut self, request: &ForkThread) -> Result<ThreadVersion, Error> {
        let source = HistoryRef {
            version: request.source,
            entries: 0..request.prefix_len,
        };
        let entries = self.entries(&source)?.to_vec();
        self.insert(request.thread, entries, vec![source])
    }
}

impl History {
    fn entries(&self, version: ThreadVersion) -> Result<&[Entry], Error> {
        let revision = usize::try_from(version.revision.0).map_err(|_| Error::NotFound(version))?;
        let length = self.lengths.get(revision).ok_or(Error::NotFound(version))?;
        Ok(&self.current.entries[..*length])
    }

    fn read(&self, version: ThreadVersion) -> Result<Thread, Error> {
        let entries = self.entries(version)?.to_vec();
        Ok(Thread::from_parts(
            version,
            entries,
            self.current.sources.clone(),
        ))
    }

    fn append(&mut self, request: &AppendEntries) -> Result<ThreadVersion, Error> {
        validate_append(&self.current, request)?;
        let Revision(revision) = self.current.version.revision;
        let revision = revision
            .checked_add(1)
            .ok_or_else(|| Error::InvalidRequest("thread revision is exhausted".into()))?;
        self.current.entries.extend_from_slice(&request.entries);
        self.current.version.revision = Revision(revision);
        self.lengths.push(self.current.entries.len());
        Ok(self.current.version)
    }
}

fn validate_append(current: &Thread, request: &AppendEntries) -> Result<(), Error> {
    if current.version() != request.expected {
        return Err(Error::Conflict {
            expected: request.expected,
            actual: current.version(),
        });
    }
    if request.entries.is_empty() {
        return Err(Error::InvalidRequest(
            "an append must contain at least one entry".into(),
        ));
    }
    Ok(())
}

//
// Publication and receipts
//

impl State {
    fn apply(&mut self, mutation: Mutation) -> Result<ThreadVersion, Error> {
        let operation = mutation.operation();
        if let Some(record) = self.operations.get(&operation) {
            return record.replay(&mutation);
        }
        let result = self.execute(&mutation);
        self.operations
            .insert(operation, Record::new(mutation, &result));
        result
    }

    fn execute(&mut self, mutation: &Mutation) -> Result<ThreadVersion, Error> {
        match mutation {
            Mutation::Create(request) => self.create(request),
            Mutation::Append(request) => self.append(request),
            Mutation::Fork(request) => self.fork(request),
        }
    }

    fn cancel(&mut self, operation: OperationId) -> OperationStatus {
        self.operations
            .entry(operation)
            .or_insert(Record {
                mutation: None,
                status: OperationStatus::Cancelled,
            })
            .status
            .clone()
    }
}

impl Record {
    fn new(mutation: Mutation, result: &Result<ThreadVersion, Error>) -> Self {
        let status = match result {
            Ok(version) => OperationStatus::Committed(*version),
            Err(error) => OperationStatus::Rejected(error.clone()),
        };
        Self {
            mutation: Some(mutation),
            status,
        }
    }

    fn replay(&self, mutation: &Mutation) -> Result<ThreadVersion, Error> {
        if self
            .mutation
            .as_ref()
            .is_some_and(|original| original != mutation)
        {
            return Err(Error::OperationConflict(mutation.operation()));
        }
        match &self.status {
            OperationStatus::Committed(version) => Ok(*version),
            OperationStatus::Cancelled => Err(Error::Cancelled(mutation.operation())),
            OperationStatus::Rejected(error) => Err(error.clone()),
        }
    }
}

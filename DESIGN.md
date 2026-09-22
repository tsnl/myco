# Architecture and interfaces

Proposed contracts for the rewrite, implemented in the review sequence below.

## Modules

The engine is one `myco` library crate. The server and remote workers are binary
targets of the same package. The browser GUI is a separate Yew application.

```text
src/
  lib.rs
  model/                       # Inference client, types, and private drivers
  thread/                      # History, fixed references, storage, and forks
  logic/
    agent.rs                   # Conversation and tool loop
    compact.rs                 # Summarization and continuation
    kernel.rs                  # Workspaces, routing, supervision, and lifecycle
  service/
    terminal_service.rs
    filesystem_service.rs
  api/                         # HTTP handlers and versioned wire schemas
  bin/
    myco-server.rs
    myco-terminal-worker.rs
    myco-filesystem-worker.rs
```

```mermaid
flowchart LR
    subgraph Server["Server / myco"]
        server[myco-server] --> http[api::http]
        http --> kernel[logic::kernel]
        kernel --> agent[logic::agent]
        agent --> compact[logic::compact]
        agent --> thread
        agent --> model
        compact --> thread
        compact --> model
        kernel --> thread
        kernel --> model
        thread[thread]
        model[model]
        kernel --> terminal[service::terminal_service]
        kernel --> filesystem[service::filesystem_service]
    end
    subgraph Protocol
        protocol[api::protocol]
    end
    subgraph Client
        gui[myco-gui / Yew]
    end
    subgraph Integrations[API clients]
        scripts["Scripts / other applications and services"]
    end
    http --> protocol
    gui -. HTTP / event streams .-> server
    scripts -. HTTP / event streams .-> server
```

`model`, `thread`, and `service` do not depend on each other or on `logic`.
Workflow modules compose them; `logic::kernel` constructs dependencies and runs
workflows. `api` calls the kernel's Rust API. Most implementation details remain
private or `pub(crate)`; module interfaces and review enforce dependency direction.

The kernel exposes Rust operations for agents, threads, workspaces, service
controls, and observation streams without an HTTP listener. `myco-server` starts
the kernel and HTTP adapter. The GUI consumes the wire API without importing the
native engine. Browser control can later join `service` as another capability.

Scripts and other applications use the same versioned HTTP API and event streams
as the GUI. Thread creation, input submission, cancellation, service controls, and
observations are available without a browser or interactive client.

Services have independent APIs suited to their capabilities. They do not depend
on thread vocabulary or the tool catalog, and need no common service trait.

Model-facing tools live under `logic::kernel`: definitions, argument schemas,
and adapters that call service APIs or internal kernel operations and translate results. GUI
controls also use service APIs through the kernel and server, sharing the same
instances and observations.
`GenAiClient::generate` validates and encodes the request synchronously, returning
`Result<Generation<'_>, Error>`. A valid request produces a concrete stream
implementing `Stream<Item = Result<Event, Error>>`; network I/O waits for polling.
It yields the request before dispatch, ordered progress, and one
`Completed { message, finish, usage }` after validation. The caller can
persist each item before polling again. Backend dispatch uses a private `Driver`
trait; there is no public model trait. The model module does not commit turns.
The returned assistant message holds content and opaque continuation JSON, ready
to append to the next request. Callers preserve continuation unchanged; private
backends interpret and validate it. Finish reason and usage describe the generation.

## Workspaces and service APIs

A workspace contains multiple agents, independent threads, and shared service
bindings on one or more hosts. Agents, including subagents, progress concurrently.
Each thread has its own writer; agents keep references to the threads they use.
A session or GUI may group related threads without making that grouping part of
the threads API.

The kernel registers service instances within workspaces. Discovery lists instance
IDs, service kinds, and API versions; resolution checks the workspace and expected
kind before returning a bound client. A binding identifies the instance and its
configuration, including host and working directory/root where applicable.
Different services keep their own request/result types. The kernel maps operation
IDs into service records and pins bindings for retries; discovery never silently
substitutes another instance for an unavailable target.

Kernel tools can create, fork, read, submit input to, or request work on another
thread in the workspace. A subagent tool starts an agent on a new thread and
records its relationship to the requesting operation. Creation is deduplicated
by operation ID; input acceptance and the eventual reply are separate observations.
These tools call kernel operations directly. Inference uses `model::GenAiClient`.

### Hosts and remote transport

Local host services run in-process. A remote service instance lazily starts its own
worker through a noninteractive SSH subprocess. The package provides
`myco-terminal-worker` and `myco-filesystem-worker` executables alongside their
service modules. Each service client owns its remote subprocess, pipe, framing, and
reconnection logic behind the same typed API. The kernel manages the lifetime of
the clients; agents and GUI clients share them across calls.

Each worker exposes its service's methods, independently of model-facing tools
in the kernel. The service owns its worker protocol and codecs, separate from the
public HTTP schemas in `api::protocol`. SSH stdio has no PTY; requested terminals
allocate PTYs inside the terminal worker. Standard output carries protocol frames,
and diagnostics use standard error. The handshake verifies the service instance
and compatible worker/protocol versions. Gen AI uses its configured inference
endpoints from the server; agents, model-provider credentials, and conversation
state remain there.

Separate pipes can still share one underlying SSH connection using OpenSSH's
configured [`ControlMaster`](https://man.openbsd.org/ssh_config#ControlMaster).
Each SSH channel has its own flow-control window
([SSH channel protocol](https://www.rfc-editor.org/rfc/rfc4254.html#section-5)).
Myco works with independent connections when sharing is unavailable and does not
manage a private SSH master initially. Shared transport still means shared network
failure and bandwidth; separate workers isolate service process failures.

Each service protocol still needs bounded frames, request IDs for concurrent calls,
chunked bulk data, and backpressure. It must keep cancellation responsive among its
own operations. Request IDs identify waits on the current pipe; stable operation
IDs correlate durable records across reconnects. Closing one observation or
cancelling one call does not close the service pipe. Kernel bindings scope service
instances to workspaces; transport sharing never merges their namespaces.

In the first implementation, each worker follows its SSH stdio lifetime and
attempts supervised shutdown on disconnect. Connection loss leaves outstanding
outcomes unknown; it does not prove that a command or edit failed. Reconnect checks
durable operation records before resubmission. Terminal survival across a lost
worker is not promised.

### Terminal

`TerminalClient` addresses a workspace-bound service instance. Processes have
stable `ProcessId`s independent of conversation threads:

```rust
impl TerminalClient {
    pub async fn start(&self, request: StartProcess) -> Result<ProcessId, TerminalError>;
    pub async fn control(&self, request: ControlProcess) -> Result<ControlReceipt, TerminalError>;
    pub async fn read(&self, request: ReadOutput) -> Result<OutputPage, TerminalError>;
    pub async fn list(&self) -> Result<Vec<ProcessInfo>, TerminalError>;
    pub async fn inspect(&self, process: ProcessId) -> Result<ProcessInfo, TerminalError>;
    pub async fn operation(&self, id: OperationId) -> Result<OperationRecord, TerminalError>;
}
```

`StartProcess` supplies an operation ID, command, working directory, environment,
and pipe or PTY mode. Start returns after durable acceptance; process exit is
observed later. `ControlProcess` carries its own operation ID and a command:
write input, resize a PTY, signal, close/reap, or cancel a target operation.
Cancellation can arrive before start; repeated identical requests reuse their
records. Writes are serialized per process, and receipts report partial delivery.
An input receipt does not imply that a command finished.

`ReadOutput` supplies a cursor, byte limit, and bounded wait. `OutputPage` contains
bytes tagged by stream, the next cursor, retention gaps, process status, and
end-of-output status; PTY output is merged. Readers have independent cursors, so
a GUI and several agents do not consume each other's output. Slow readers cannot
block process draining. Exit status includes code or signal; an idle wait is not
an exit.
Lost workers require reconciliation, not a claim that process state was restored.

The kernel's `bash` adapter builds finite execution and interactive waits from
these operations. GUI terminals use the same process IDs and output records.
Processes belong to the workspace and survive HTTP client disconnects and
compaction.

### Filesystem

`FilesystemClient` exposes file operations independently of model-facing tool schemas:

```rust
impl FilesystemClient {
    pub async fn read(&self, request: ReadFile) -> Result<FileContent, FilesystemError>;
    pub async fn list(&self, request: ListDirectory) -> Result<DirectoryPage, FilesystemError>;
    pub async fn create(&self, request: CreateFile) -> Result<FileVersion, FilesystemError>;
    pub async fn edit(&self, request: EditFile) -> Result<FileVersion, FilesystemError>;
    pub async fn operation(&self, id: OperationId) -> Result<EditRecord, FilesystemError>;
}
```

Reads return bounded bytes or a text range, explicit truncation, and a version of
the whole file; directory listings are paginated. Mutations carry operation IDs.
Create requires an absent path. Edit requires an expected file version and selects
literal replacement, insertion after a line, or whole-file write for GUI saves.
Replacement requires a nonempty search string with exactly one match; insertion
uses one-based lines, with zero meaning the beginning of the file.

The kernel's `str_replace_based_edit_tool` adapter maps view/create/replace/insert
onto these operations. It supplies the expected version from a recorded read or
successful mutation available to the caller. Provenance must remain
available across compaction; a prose summary alone cannot establish a file version.
GUI saves supply their own observed version. File versions are explicit API
values; the service does not maintain a conversation-specific read cache.

The service serializes edits to each resolved target, rejects stale versions, and
publishes complete files atomically while preserving permissions and symlink
targets. Version checks detect observed external changes; they cannot exclude
arbitrary writers that bypass the service. Records retain mutation intent and
before/after versions for reconciliation; an ambiguous insert is not blindly
replayed. Filesystem and terminal bindings can address the same host/files.

## Thread history

A thread is an identified, ordered, append-only conversation. Existing entries
and published revisions are immutable; appending produces a new revision under
the same ID. A fork gets a new ID and copies a fixed prefix. A reference used as
generation context pins a revision and range, so later appends cannot change it.

```rust
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ThreadVersion {
    pub thread: ThreadId,
    pub revision: Revision,
}

#[derive(Clone)]
pub struct HistoryRef {
    pub version: ThreadVersion,
    pub entries: std::ops::Range<usize>,
}

#[derive(Clone)]
pub struct Thread {
    version: ThreadVersion,
    entries: Vec<Entry>,
    sources: Vec<HistoryRef>,
}

impl Thread {
    pub fn version(&self) -> ThreadVersion;
    pub fn entries(&self) -> &[Entry];
    pub fn sources(&self) -> &[HistoryRef];
}
```

Owned values use dense vectors. Cloning copies that value's entries and provenance,
preserving identity and revision; it grants no writer and does not recursively
copy referenced threads. The kernel chooses the storage format.

`thread` owns the canonical conversation entries: user and assistant messages,
tool results, system information, warnings, errors, and notifications. Its content
types are independent of inference types. A stored entry need not appear in a
model prompt. Workflow code chooses how to render it or retain it only for humans.
Thread data has no agent policy, foreground selection, or live operation state.

`thread::Threads` exposes history operations through an injected store:

```rust
impl Threads {
    pub fn new(store: Arc<dyn ThreadStore>) -> Self;

    pub async fn create(&self, request: CreateThread) -> Result<ThreadVersion, ThreadError>;
    pub async fn read(&self, version: ThreadVersion) -> Result<Thread, ThreadError>;
    pub async fn append(&self, request: AppendEntries) -> Result<ThreadVersion, ThreadError>;
    pub async fn fork(&self, request: ForkThread) -> Result<ThreadVersion, ThreadError>;
}
```

Mutation requests carry an operation ID and the expected revisions of existing
threads. `ThreadStore` is a narrow, dyn-compatible storage interface defined by
`thread`. It loads history and atomically publishes mutations with their receipts.
The kernel supplies its implementation. Create and fork return new identities;
append preserves identity and advances the revision.

Publication is serialized per thread. Reading and forking published history do
not wait for inference. Workflow code decides whether to queue competing input
while it generates; storage checks revisions regardless of that policy. A cloned
snapshot grants no writer. A stale append cannot silently attach to newer history.

## Workflow composition and streaming

Workflow code constructs a model request, consumes its stream, validates the
outcome, and appends accepted entries through `Threads`. Inference and history
publication are separate operations. The shared history API serves several uses:

| Workflow | Composition |
| --- | --- |
| Interactive agent | Append input, read context, generate, append the reply, execute tools, append results, repeat. |
| Background work | Use the same operations under event/schedule triggers and an application stopping policy. |
| Beam search/evaluation | Fork a fixed prefix, generate candidates concurrently, record outcomes, grade, and select. |
| Compaction | Read pinned history, generate in a summarization thread, create a continuation from its result and retained turns. |
| Cross-thread summary | Read several fixed histories and publish a summary in another thread. |

Search candidates isolate tool workspaces or defer tool effects until selection.
A session can group threads; background work can create successive threads without
an enclosing agent object.

`model` has its own inference input, content, and incremental-part vocabulary.
Its conversational roles are user and assistant; backends encode structured tool
calls/results in their provider's format. System instructions are request fields.
The richer roles in `thread` are interpreted by each workflow, not mechanically
converted into model roles. Opaque continuation data stays in inference records
alongside the portable thread entries; workflows need no provider-specific types.

All generation increments belong to one logical turn. Workflow observation streams
can expose:

```rust
pub enum TurnUpdate {
    Delta(TurnDelta),
    Committed(Turn),
}
```

`TurnUpdate` belongs to `logic`, not the history API. Deltas carry generation/
attempt identity and content-block coordinates, including incomplete tool arguments.
`model::Event::Completed { message, finish, usage }` supplies a validated inference
outcome. Workflow code records its evidence, checks conversation structure and correlation, and
appends it with the expected revision and operation receipt before yielding
`Committed`. A refusal or output limit can be recorded as such; incomplete
arguments never authorize tool execution.

The model stream yields one completed response or terminal error, then ends.
EOF without a terminal provider outcome is an error. A workflow may fail after
inference succeeds, for example when its append conflicts. It must not report
that candidate as committed. Stale candidates remain available in attempt records.

The caller owns polling and backpressure. Dropping a pending `next()` wait leaves
the stream available for subsequent polling. Dropping the owning generation stream
releases its local attempt; operation records preserve unresolved work for
reconciliation. This does not prove the remote provider stopped computing.

Workflows consume generation streams and forward updates through their own streams.
The kernel owns those streams and records observations before distributing them
to GUI and HTTP subscribers. Subscribers have bounded queues, cursors, and an
explicit recovery path for gaps. Disconnecting a browser drops its subscription,
not the operation that the kernel is supervising.

Each inference call represents one attempt. Retry policy belongs to the workflow,
with explicit attempt boundaries after visible progress. Retrying an ambiguous
operation first reconciles its record. Resampling and tool strategy are also
application policy; output from different attempts is never silently concatenated.

## Agents as application code

`logic::agent` composes history, inference, tool execution, and agent-state
storage. Effectful dependencies return results directly to the awaiting function;
tests can inject scripted implementations at these boundaries. Small async helpers
implement the tool loop, budgets, and stopping conditions. Compaction is factored
into `logic::compact` so other workflows can also use it.

```rust
impl Agent {
    pub fn run(&mut self, input: Input) -> AgentRun<'_>;
}

pub enum AgentUpdate {
    Generation { thread: ThreadId, update: TurnUpdate },
    Tool(ToolObservation),
    Finished(AgentReply),
}
```

`AgentRun` implements `Stream<Item = Result<AgentUpdate, AgentError>>`.
It appends input, prepares model context, consumes a generation stream, forwards
deltas, publishes the accepted turn, executes validated tool calls, appends results,
and repeats as policy requires. An active run exclusively borrows its agent through
`&mut self`. Distinct agents run concurrently.

Agent state contains its selected thread references, policy/budget state, and
pending operation IDs. Injected storage records explicit logical checkpoints.
The threads remain independently addressable. Forking an agent creates new
thread identities from fixed histories and copies the relevant policy state;
it does not clone a running stream, future, tool process, or writer.

The kernel polls concurrent agent streams as updates become ready. Ordinary
async scheduling advances their I/O; it need not reconstruct a function on every
event. Pending streams stay alive when another agent yields. Polling remains
bounded, and cancellation/control input cannot be starved by progress.
Application code must yield during long computation.

## Context, compaction, and interpretation

```rust
pub struct GenerationIntent {
    pub source: ThreadVersion,
    pub instructions: String,
    pub context: Vec<HistoryInput>,
}

pub enum HistoryInput {
    Inline(HistoryRef),
    Readable { name: String, history: HistoryRef },
}
```

These context types belong to `logic`. Each workflow chooses instructions and
context, renders the target conversation, and resolves additional history.
Inline history is quoted source material. Readable history is advertised through a history-reading tool;
its display name may resolve to a path, but its identity remains the fixed reference.
Only advertised, workspace-authorized references can be read through that tool.

`logic::compact` pins ranges from one or more source
threads and a suffix of complete turns to retain, creates a summarization thread,
runs generation/tool helpers there, and creates a continuation thread from the
summary and retained turns. Source and worker histories remain available.
Cuts preserve complete tool invocation/result groups.

Sources can continue to grow because compaction reads fixed revisions. Selecting
the continuation and resuming work there are explicit policy decisions, conditional
on the expected source/selection state. A late summary cannot replace newer work.
Failure before publication leaves the sources intact; cancellation committed first
prevents publication. Already published history remains available.

Provider request/response types stay in `model` and its callers in `logic`.
Workflow adapters pin model configuration and capabilities, record the exact
request before polling the inference stream into dispatch, and translate progress
and its final outcome into thread values.

Native continuation, raw provider metadata, and provider-call/invocation-ID
mappings remain in interpreter records. Evidence is durable before a thread or
operation can reference it. Retained history keeps evidence reachable.
Continuation is reusable only when it represents the selected context; otherwise
the adapter reconstructs the request or reports an incompatibility.

Tool adapters validate against pinned schemas, invoke a service or internal kernel
operation, and record a translated outcome. GUI controls use those same service
instances through direct kernel operations.

## Persistence, cancellation, and lifecycle

Each effectful API records intent before dispatch and retains stable operation
IDs across retries. Thread mutations publish history and their receipts atomically.
Service operations also retain their own records. An uncertain response triggers
lookup/reconciliation of the existing operation, not a fresh submission under
a new ID. Request deduplication cannot guarantee exactly-once external effects.

Generation's final append checks its expected revision and cancellation state in
the same transaction. A lost response after commit is recovered from its receipt.
Partial streamed output stays in attempt evidence unless explicitly accepted as
an incomplete turn; it is never mistaken for a finished reply.

Cancellation is an explicit request to the supervised operation. It prevents
undispatched work and requests interruption of active work. Commit order resolves
completion races. Acknowledgement does not undo tool side effects or settle an
unknown outcome. Dropping an observer or an idle wait is not an explicit cancel.

Recovery loads threads, agent checkpoints, and operation records, reconciles
pending work, then resumes the application at a defined logical boundary. A
suspended Rust future is never the persistence format. Applications define their
restart policy; arbitrary function-stack restoration is not promised. Tests use
scripted dependencies and retained observations without requiring deterministic
re-execution of all application code.

The kernel resolves workspace bindings, restores agents, reconciles operations,
and accepts new work. Shutdown stops intake, finishes or reconciles mutations,
applies cancellation policy, and closes service clients. Each service manages its
workers. The server starts the kernel and HTTP listeners and forwards shutdown
signals; Rust applications can manage the kernel directly.

The HTTP API covers workspaces, agents, threads, input/cancellation, service
controls, and observation streams. Mutations carry deduplication IDs; acceptance
and completion are separate. The Yew GUI and scripted clients use the same API.
Workspace membership alone provides no filesystem isolation.

## Evaluation and review sequence

Evaluations inject scripted inference, an in-memory store, and a recording tool
executor, then inspect updates and resulting histories. Real trials use service
adapters with budgets, isolated workspaces, and graders. Neither requires HTTP.
GEPA varies prompts or agent policy and consumes trial scores, traces, and
diagnostic feedback.

Review steps are module-sized changes within the engine crate.

1. **Interfaces:** module boundaries, history operations, workflow composition,
   stream completion, persistence, cancellation, and recovery.
2. **Model:** `model::GenAiClient`, private drivers, and a concrete stream.
   Check request-before-dispatch, ordered progress, explicit completion, native
   continuation, consumer backpressure, concurrent requests, and stream drop.
3. **Thread:** owned vector histories, rich entries, fixed references, injected
   storage, and atomic append/fork. Check stale writes, independent forks,
   ambiguous commits, and cancellation/publication races.
4. **Agent/compaction logic:** request projection, streamed generation and turn
   publication, tool loops, checkpoints, compaction, and concurrent runs. Use
   scripted dependencies to check complete tool groups, retries, budgets,
   cancellation, compaction failure, and restart at logical boundaries.
5. **Terminal service:** shared processes, output cursors, deduplication, and
   local/SSH backends. Check independent readers, input ordering, PTY controls,
   concurrent replies, backpressure, and worker loss.
6. **Filesystem service:** bounded reads, versioned create/edit, operation records,
   and local/SSH backends. Check stale edits, ambiguous matches, symlink targets,
   create conflicts, and recovery after interrupted writes.
7. **Kernel logic:** storage and service adapters, discovery, agent supervision,
   observation delivery, and shutdown. Check provider/thread translation, several
   agents per workspace, internal delegation, and subscriber reconnects.
8. **HTTP API and GUI:** HTTP schemas/streams exercised by scripted clients,
   then thread browsing, conversation grouping, and shared service controls in Yew.
9. **Evaluation/GEPA:** isolated trial fixtures and inspectable optimizer feedback.

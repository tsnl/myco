# Scripted sessions and evals

For session-derived cases, repeated model comparisons, unattended runs, and
GEPA prelude optimization, use `myco-eval` ([manual](src/manual/articles/evals.md)).
The APIs below are the underlying workflow for custom embedders.

`SessionRunner` runs the same durable workflow used by server sessions.
It owns an `Agent`, a session-bound `SessionRuntime`, and compaction policy. Supply
a `GenerativeModel` and `EventSink`; the runtime routes tools through a `Harness`.
Use `Harness::local_with_services` to add fixture services, or use the lower
`myco-agent` crate directly with a supplied `ToolExecutor` for an in-memory eval.

Application models should be wrapped with `core::image_store::with_images(model,
ImageStore::for_profile()?)`. `SessionRunner` and `SessionRuntime` externalize
image input and tool results; the wrapper resolves references only for provider
requests. Protocol drivers reject unresolved references. Session JSON and the
profile's `images/` directory form one portable store.

```rust,ignore
let session = ActiveSession::new(Session::new("candidate"));
let runtime = SessionRuntime::new(harness, session);
let agent = Agent::new(model.clone(), runtime.clone(), sink);
let mut runner = SessionRunner::new(agent, runtime).await?;
runner.set_model(model, ModelInfo::named("candidate")).await?;
runner.set_compactor(compactor, Some(80_000));
let outcome = runner.submit(input, Utc::now(), cancel).await;
let completed = outcome.result?;
grade(completed.answer, completed.reason, completed.usage);
```

Use `ModelInfo::from_spec` for a catalog model. Model identity excludes credentials;
change models through `SessionRunner::set_model` so the change is recorded before
further execution. This also invalidates usage measured under a different model.
Configure the agent's context window, retry policy, and truncation cap to match
the supplied model. `agent_mut()` exposes those lower-level controls; direct model
changes there bypass the runner's identity bookkeeping.

`ModelCompactor { model }` uses a catalog model with history access restricted to
the assigned thread and one summary write. It stops after 12 model requests or
120 seconds. `CompactionProgress` reports elapsed time every 10 seconds.
A supplied `Compactor`
can implement deterministic summaries or another evaluation policy. Automatic
compaction works between tool rounds and after normal answers. It retains the
runtime owner, run usage, and truncation cap. Manual `compact` only commits the new
context. A summary that fails to reduce context below the threshold disables
automatic compaction until manual compaction succeeds or the session changes.

`RunOutcome` reports the final answer, stop reason, and measured main-agent usage.
Output tokens sum across generations, including automatic continuations; input
tokens describe the last measured prompt. Usage covers reported tokens only and
can undercount when providers omit reports. It excludes a separate compaction
worker's requests. Capture `AgentEvent`s
with an `EventSink` and `WorkflowEvent`s with `set_observer` for run diagnostics.
Grade actual tool outcomes or artifacts as well as the answer. Use a fresh home,
runtime, and task workspace for each eval case; fixed fixtures make regressions
repeatable, while candidate model calls measure real task quality.

The runnable [offline example](examples/scripted_session.rs) injects both a model
and compactor, uses real local bash sessions, checks an artifact and usage, and
reloads from disk. Replace its fixture model with a catalog-backed implementation
to evaluate providers. It leaves the session and artifact under the printed
temporary path. It makes no model-provider calls.

## Persistence and recovery

All operations share the `ActiveSession` writer gate. Independent processes using
the same store must also hold `session::SessionWriteLock`, as the server does. A
checkpoint saves intent before effects and observations before further work.
Save failures stop execution and retain newer in-memory observations. After
repairing storage, `continue_run` retries the current boundary without adding
input or repeating completed effects. Dropping an in-flight future makes its
outcome unknown: call `agent_mut().recover_interrupted()` before continuing.
Cancel cooperatively with `CancelToken` when possible so tools can acknowledge
cleanup. Neither an unknown result nor a saved transcript proves that an external
effect stopped.
An older agent's checkpoint is rejected once another writer takes over the session;
construct a new runner from the saved context rather than overwriting newer work.
`submit`, `resume`, and `continue_run` return the same `SessionTurnOutcome`, including
any human input removed after a provider size rejection.

Load `Session`, create a new `ActiveSession` and `SessionRuntime`, then construct
a runner to resume after restart. Construction reconciles pending tool batches
as unknown, never by replaying them. `submit` appends actual input; `resume` adds
a hidden continuation and drives the existing task without a human timestamp.
Server `--resume` opens history and waits for input instead of invoking this method.

Hidden `Content::System` parts store session identity, continuations, compaction,
recovery, and `RuntimeRecord` observations. The model receives their text in order;
shell transcripts omit them. A runtime record includes model/effort, tool inventory,
and handles unavailable after restart. Inventory queries never wake lazy remotes;
query errors preserve clearly marked last-known data. Handles and editor read
fingerprints are not restored. Compaction archives original observations and
carries current runtime facts into its successor.

For finer scheduling without the session layer, use `Agent::start_run` and `step`.
Each step interprets one generation or concurrent tool batch. `AgentState` provides
the same transitions synchronously for deterministic checks. Context replacement
between generations must use `replace_at_boundary`; it preserves run policy and
invalidates the old operation identity. An executing operation cannot be replayed
or replaced after its future is dropped without explicit reconciliation.

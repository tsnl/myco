# myco-agent

Headless agent execution over a supplied generative model, tool executor, and
event sink. `Agent::run` advances existing context; callers own input submission,
session binding, resource lifetime, and persistence.

`AgentState` is the pure controller used by `Agent`. Its synchronous transitions
return `Effect::Generate`, `Effect::ExecuteTools`, or a terminal result. Callers
can feed scripted responses directly without an async runtime, or let `Agent`
interpret effects using real model and tool capabilities. Every pending effect
has an operation identity; stale and duplicate completions are rejected. Context
replacement and rewind reject unmatched tool calls and edits during pending work.

`Agent::run_with_outcome` includes the model stop reason and run usage, so an
evaluator can distinguish an output-cap stop from a completed response. Missing
usage stays unknown. Input and context editing methods return errors instead of
accepting malformed histories.

`ToolExecutor` provides a catalog and asynchronous dispatch. Calls in a tool round
run concurrently, while their recorded results retain call order. Cancellation
settles matched tool-call/result pairs before generation resumes.
When cancellation cleanup times out, the result explicitly records unknown
effects. A transcript does not establish whether those effects stopped.

Dispatch receives separate cancellation and background-request tokens.
`ToolStarted` exposes the latter to frontends together with a unique call ID;
`ToolFinished` carries the same ID. Supporting executors can release a foreground
wait and return a retained-work handle without cancelling that work. Executors
that cannot background their tools may ignore the background token.

The fallible `Checkpoint` callback receives `AgentState` before each effect and
after completion. Persist its history, usage, and `pending_operation` together.
An error stops further effects. `recover_checkpoint` converts interrupted tool
batches to unknown outcomes without dispatching them; save that recovered
context before generating. Pending tool histories are valid checkpoints but
cannot be sent to a model until their results have been reconciled.

`Content::System` carries runtime instructions and structured metadata. Providers
receive its text in message order; user-facing transcript renderers omit it.
Continuation after output truncation uses this part and does not create a human
submission.

An optional `BeforeGenerationNotice` callback supplies a pending runtime notice before
each generation step. Its text is attached to the latest user input or tool result
and checkpointed before generation. The system prompt stays fixed; retries reuse
the same input, and cancellation can interrupt polling for a notice.

`start_run` and `step` yield between individual generations and concurrent tool
batches. `replace_at_boundary` swaps settled model context while retaining run
usage and truncation counters. A dropped step cannot be dispatched again without
explicit `recover_interrupted`; it records unknown tool outcomes. After a failed
checkpoint, `continue_run` retries persistence and continues preserved live state.
`checkpoint` retries only persistence. Callers can reject superseded writers in
their checkpoint callback; the application session runner does so.

Run `cargo test -p myco-agent` for the standalone execution and cancellation tests.

The [agent guide](https://tsnl.github.io/myco/developers/agents.html) covers
embedding, cancellation, and persistence; the
[API reference](https://tsnl.github.io/myco/api/myco_agent/) lists the public types.
Run `cargo run -p myco-agent --example headless` for an offline example using a
supplied model and an in-memory tool executor.

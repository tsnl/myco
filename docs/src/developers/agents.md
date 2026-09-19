# Agent API

`myco-agent` drives model context through generation and tool rounds.
The extracted crate is usable today; higher-level service and orchestration
interfaces are still evolving. It does not start a CLI, create sessions, read
project guidance, install tools, or persist anything by itself.

The core interfaces are [`Agent`](../api/myco_agent/struct.Agent.html),
[`ToolExecutor`](../api/myco_agent/trait.ToolExecutor.html), and
[`EventSink`](../api/myco_agent/trait.EventSink.html).

## Supply an environment

An executor advertises tools and dispatches a call asynchronously. It owns
resource scope and cleanup. Unknown tools, invalid input, and ordinary tool
failures should return `ToolResult::err`; an advertised JSON schema does not
replace validation inside your implementation.

The headless example uses an in-memory echo tool:

```rust
{{#include ../../../crates/myco-agent/examples/headless.rs:tools}}
```

Calls in one tool round execute concurrently, while recorded results retain
call order. Synchronize any shared mutable state in the executor. Implement
cancellation for external work; dropping a future alone does not necessarily
stop a subprocess or undo a remote operation.

## Assemble and run

The example's `DemoModel` implements `GenerativeModel` with a deterministic
stream: it requests the echo tool, then turns the result into an answer.
It makes no network calls. You can replace it with a real driver from
`myco_model::new`, supplying `tools.tool_specs()` in that driver's config.

```rust
{{#include ../../../crates/myco-agent/examples/headless.rs:run}}
```

Run the complete, compiled example:

```bash
cargo run --locked -p myco-agent --example headless
```

The full source is
[`crates/myco-agent/examples/headless.rs`](https://github.com/tsnl/myco/blob/main/crates/myco-agent/examples/headless.rs).
It imports only the two library crates and ordinary async/JSON dependencies.

`append_input` appends and checkpoints input. `replace_context(history, usage)`
installs existing context without changing tool state or emitting a checkpoint.
`run(cancel)` advances the existing context until the turn ends; it does not
add a user message. Its answer is the final generation's text/image content,
while `history()` retains the intervening tool rounds.

## Configure policy explicitly

When embedding a configured driver, propagate settings into the agent:

- `set_retry_policy(backend.retry_policy())` for the gateway's retry policy.
- `set_context_window_tokens(spec.context_window_tokens)` for the budget exposed
  to callers; this setter does not enforce a limit or compact history.
- `set_max_truncated_resumes(spec.max_truncated_resumes)` for bounded output
  continuation. This is not a cap on the number of tool rounds.

The model's advertised catalog and your executor must agree. `set_tools`
changes execution only; it does not update a previously constructed model's
tool schemas. Rebuild and replace the model when its advertised tools change.

## Persist effects and observations

`set_checkpoint` installs a synchronous callback receiving `AgentState` and
returning `Result<(), String>`. It runs before effects begin and after their
observations settle, including the final answer. Persistence failure stops
execution; callbacks should complete their atomic write before returning.
Your storage layer owns locking, schema versioning, and stale-writer rejection.

Persist the history, usage, and `pending_operation` together. A checkpoint with
pending tool calls records uncertain external effects and is not valid model
input. `recover_checkpoint` records unknown outcomes for those calls;
`replace_context` validates complete call/result pairs before accepting history.
`start_run` and `step` expose individual generations and tool batches for callers
that need to compact or schedule work at settled boundaries. Myco's
`SessionRunner` supplies the application persistence and recovery workflow.

## Cancellation and observation

Cancel a clone of `CancelToken` and **await the run future** so cleanup can
finish. During tool execution the agent gives dispatches a short cleanup grace
period, then supplies cancelled results for unfinished calls. This preserves
tool-call/result pairing. Forcefully aborting the Tokio task bypasses this
cooperative completion path.

An `EventSink` receives text/thinking deltas, tool starts, failure/retry notices,
and `TurnFinished`. `TraceContext` attributes events to an agent and optional
session/thread labels. The sink is synchronous, so avoid blocking I/O in `emit`.
`TurnFinished` is emitted after `run` completes on success, error, or cooperative
cancellation; inspect the returned result to determine which occurred.

Events currently omit tool results and are not a durable replay format.
Use complete history and executor-owned observations when you need those data.
For Myco's existing persistent composition, see [application architecture](architecture.md).

# myco-agent

Headless agent execution over a supplied generative model, tool executor, and
event sink. `Agent::run` advances existing context; callers own input submission,
session binding, resource lifetime, and persistence.

`ToolExecutor` provides a catalog and asynchronous dispatch. Calls in a tool round
run concurrently, while their recorded results retain call order. Cancellation
leaves matched tool-call/result pairs at checkpoint boundaries.

An optional `BeforeGenerationNotice` callback supplies a pending runtime notice before
each generation step. Its text is attached to the latest user input or tool result
and checkpointed before generation. The system prompt stays fixed; retries reuse
the same input, and cancellation can interrupt polling for a notice.

Run `cargo test -p myco-agent` for the standalone execution and cancellation tests.

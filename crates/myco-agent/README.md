# myco-agent

Headless agent execution over a supplied generative model, tool executor, and
event sink. `Agent::run` advances existing context; callers own input submission,
session binding, resource lifetime, and persistence.

`ToolExecutor` provides a catalog and asynchronous dispatch. Calls in a tool round
run concurrently, while their recorded results retain call order. Cancellation
leaves matched tool-call/result pairs at checkpoint boundaries.

An optional `ContextRefresh` callback checks external context before each generation
step. Its note is attached to the latest input and checkpointed before generation;
retries reuse the same context, and cancellation can interrupt the refresh.

Run `cargo test -p myco-agent` for the standalone execution and cancellation tests.

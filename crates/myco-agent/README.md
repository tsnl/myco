# myco-agent

Headless agent execution over a supplied generative model, tool executor, and
event sink. `Agent::run` advances existing context; callers own input submission,
session binding, resource lifetime, and persistence.

`ToolExecutor` provides a catalog and asynchronous dispatch. Calls in a tool round
run concurrently, while their recorded results retain call order. Cancellation
leaves matched tool-call/result pairs at checkpoint boundaries.

Run `cargo test -p myco-agent` for the standalone execution and cancellation tests.

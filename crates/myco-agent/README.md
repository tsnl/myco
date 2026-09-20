# myco-agent

Headless agent execution over a supplied generative model, tool executor, and
event sink. `Agent::run` advances existing context; callers own input submission,
session binding, resource lifetime, and persistence.

`ToolExecutor` provides a catalog and asynchronous dispatch. Calls in a tool round
run concurrently, while their recorded results retain call order. Cancellation
leaves matched tool-call/result pairs at checkpoint boundaries.

Run `cargo test -p myco-agent` for the standalone execution and cancellation tests.

The [agent guide](https://tsnl.github.io/myco/developers/agents.html) covers
embedding, cancellation, and persistence; the
[API reference](https://tsnl.github.io/myco/api/myco_agent/) lists the public types.
Run `cargo run -p myco-agent --example headless` for an offline example using a
supplied model and an in-memory tool executor.

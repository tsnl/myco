# Architecture and review steps

## Boundaries

| Component | Responsibility |
| --- | --- |
| `myco-llm` | One inference attempt against an OpenAI Responses or Anthropic Messages endpoint. Context and settings in; progress and a response out. |
| `myco-agent` | Agent behavior: threads, context assembly, compaction, events, and triggers. Model inference, tool dispatch through `Harness`, and state access are injected. |
| `myco-tool-service` | Shared tool instances, execution, worker lifetimes, observation streams, and human/agent control. |
| Server application | Workspaces, agent supervision, state implementations and persistence formats, HTTP endpoints, and application lifecycle. |
| Web GUI | Human interaction with agents and shared tools through the same server API available to machine clients. |

An agent is a durable identity with threads and execution traces. There is no
session container. The agent library defines semantic operations on state, not
an on-disk or wire serialization format. The server chooses and versions those
formats. Agent traces and tool-service records are both retained, with operation
IDs linking requests, execution, and observations. A missing observation denotes
unresolved work, not proof that an operation is running or never executed.

Pumping an event is an async operation that the caller awaits to completion.
Only one event advances a given agent at a time. Events separate requests from
responses, so an event does not wait for a later event to make progress. The
server can pump several agents concurrently. Trigger semantics belong to the
agent; delivery from clocks, watchers, and HTTP requests belongs to the server.

Live tool resources are owned by the tool service within workspace scopes, not
by agent transcripts. Agents and humans can observe and control the same
instances. Resource access and lifecycle policies are explicit. The server
supervises event pumps, workers, and shutdown; client connections do not own
agent or tool lifetimes. Machine callers invoke HTTP endpoints rather than
starting an interactive client. The first application frontend is a web GUI.

Evaluators are independent callers of the agent library. They supply candidate
configuration, initial state, environment fixtures, budgets, and graders. GEPA
can drive those evaluations through an adapter that returns scores, traces, and
diagnostic feedback. Trials keep exact inference inputs and outcomes, candidate
identity, and tool evidence. Workspace membership alone is not filesystem
isolation.

## Review sequence

1. **LLM boundary (current step).** A small Rust workspace containing only
   `myco-llm`. Injectable inference interface; HTTP adapters for both providers;
   streaming text and tool calls; provider continuation data; explicit incomplete
   responses and errors; request cancellation by dropping the stream. Validate
   protocol behavior with local HTTP fixtures, without API credentials.
2. **Agent state machine.** Inject state access, model inference, and `Harness`.
   Add threads and serialized event pumping, then bounded compaction. Validate
   with an in-memory state implementation and scripted dependencies. Keep storage
   encodings outside the agent crate.
3. **Shared tool service.** One workspace terminal with independent observers,
   human/agent control, execution records, cancellation, and worker supervision.
   Validate concurrent agents and stale operations after control transfer.
4. **HTTP server.** Compose both libraries, implement durable state and tool
   records, expose machine-friendly operations and observation streams, and test
   application shutdown and restart reconciliation.
5. **Web GUI.** Agent/thread browsing and a shared terminal using those APIs.
6. **Evaluation and GEPA.** Run the same agent against isolated task fixtures;
   emit inspectable results and feedback; add the optimizer adapter.

Each step is a review boundary. Later crates and application code are added only
as their steps are reached. Provider adapters and ordinary functions should earn
any shared abstraction through actual repeated behavior.

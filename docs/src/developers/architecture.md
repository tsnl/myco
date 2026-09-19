# Application architecture

The `myco` crate assembles the reusable libraries with profiles, prompts,
sessions, terminal rendering, and host tools. Depend on it when you need that
composition; `myco-model` and `myco-agent` remain sufficient for a custom
environment.

```text
CLI / SessionRunner
  ├── Agent (myco-agent)
  │     └── GenerativeModel (myco-model)
  └── SessionRuntime (ToolExecutor + live resource ownership)
        └── Harness (host routing)
              ├── local → in-process HostWorker
              └── remote → ssh … myco --mode host
```

## Keep the lifetimes separate

| Object | Owns | Lifetime |
| --- | --- | --- |
| `Agent` | Model context, usage, model and tool handles, sink | One live agent instance |
| `Session` | Metadata and ordered threads | Persistent document |
| `Thread` | Linear history and usage estimate | One context within a session |
| `SessionRuntime` | Session binding, harness, resource owner | Shared across thread or agent replacements |
| Host tool service | Processes, output buffers, read stamps | Host-local resources scoped by owner |

Retaining a `SessionRuntime` keeps live tools across compaction or agent
replacement. Starting a different session creates different tool ownership.
Reloading a saved document after process exit does not recreate those resources.

[`SessionRuntime::bind_agent`](../api/myco/session_runtime/struct.SessionRuntime.html#method.bind_agent)
binds the active thread and attribution and clears the previous checkpoint.
The chat adapter wires persistence back in. For a CLI-like turn, use
[`SessionRunner`](../api/myco/chat/struct.SessionRunner.html): it owns the agent,
session writer coordination, input submission, recovery, and compaction policy.
It preserves live tools when threads change and rejects stale checkpoints.
Lower-level `chat::run_session_turn` submits one durable turn; `chat::interact`
only appends user input and runs the agent.

## Extend tools at the right layer

Implement the agent's `ToolExecutor` for a custom environment independent of
Myco hosts. Implement the application's
[`ToolService`](../api/myco/tool_services/trait.ToolService.html) when adding a
tool to its existing host runtime. The harness adds the optional `host` field
to routed tool schemas and sends calls to the selected host.

Local is always in-process. Remotes attach lazily and use a version-checked,
concurrent NDJSON protocol. Session metadata and prelude tools are installed
only on the local worker. Model credentials stay with the application process.

## Find code by responsibility

| Area | Entry point |
| --- | --- |
| Startup and CLI controls | `src/bin/myco.rs` |
| Profiles, models, authentication | `src/config/`, `src/core/fs.rs` |
| Agent execution | `crates/myco-agent/src/lib.rs`, `generation.rs` |
| Provider translation and streaming | `crates/myco-model/src/` |
| Session turns and compaction | `src/chat/`, `src/session/` |
| Live resource binding | `src/session_runtime.rs` |
| Host transport and routing | `src/host/`, `src/harness/` |
| Tools and terminal rendering | `src/tool_services/`, `src/tui/` |

The repository's [guided code tour](https://github.com/tsnl/myco/blob/main/TOUR.md)
follows complete execution paths and points to integration tests. The
[generated reference](reference.md) provides signatures and source links.

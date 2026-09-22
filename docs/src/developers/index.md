# Choose a crate

Myco's workspace has three packages. Start with the smallest one that owns the
behavior you need:

| You want to… | Use | Bring yourself |
| --- | --- | --- |
| Call a model and consume streamed responses | [`myco-model`](inference.md) | Endpoint, credentials, model settings, messages, retry policy |
| Run a model/tool loop without the server | [`myco-agent`](agents.md) | A `GenerativeModel`, `ToolExecutor`, event sink, input, persistence |
| Reuse Myco's hosts, sessions, and tool services | [`myco`](architecture.md) | Application startup and resource ownership |

Dependencies flow from the application to `myco-agent`, then to `myco-model`.
Neither lower crate depends on the application. You do not need SSH, terminal
rendering, a Myco profile, or a session store to embed the agent.

## Use the workspace or a registry release

Within a checkout, examples use the local packages:

```bash
cargo run --locked -p myco-agent --example headless
```

In another project, select a published version available for both libraries:

```toml
[dependencies]
myco-model = "0.3"
myco-agent = "0.3"
tokio = { version = "1", features = ["macros", "rt"] }
```

The example sources also use `futures` and `serde_json`; include those when
copying the examples. To try unreleased changes, use path dependencies to
`crates/myco-model` and `crates/myco-agent` in the same checkout, or Git
dependencies pinned to the same commit.

These are evolving APIs. Workspace packages share a version and the agent
pins its model dependency exactly. Keep both libraries on a compatible release
or the same source revision. This site's [API reference](reference.md) is
built from the same commit as the guide.

## A useful reading order

1. [Inference](inference.md): messages, stream events, and one-attempt drivers.
2. [Agents](agents.md): tool execution, history, cancellation, and checkpoints.
3. [Application architecture](architecture.md): session and host composition.
4. [Evaluations](evaluations.md): a reserved home for the future eval workflow.

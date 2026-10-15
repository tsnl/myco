# Agent kernel research

Research and an executable semantic model, September 19, 2026. This directory is
an independent Cargo workspace. It does not change Myco's runtime or data format.
The proposed contracts are library design; the experiment tests selected contracts,
not production durability, provider integration, training quality, or a web UI.

The objective is a small reusable foundation for interactive agents, persistent
services, cooperating agents, evaluations, reinforcement learning, and supervised
fine-tuning. Session management and presentation should compose that foundation.

Read [DESIGN.md](DESIGN.md) for the recommendation and tradeoffs,
[SOURCES.md](SOURCES.md) for the research, and [VALIDATION.md](VALIDATION.md) for
the evidence and remaining implementation gates. `src/` is the executable model;
`tests/` contains adversarial scenarios and small interpreters.

## Working requirements

1. A headless agent can run without a session, filesystem, network, UI, or actor
   framework. Its decisions can be driven by real, simulated, or recorded outcomes.
2. Context revisions are immutable. Compaction and forks retain provenance and do
   not implicitly copy, destroy, or transfer live resources.
3. Long operations leave their owner responsive to cancellation and interjection.
   A late or duplicate reply cannot dispatch work a second time.
4. Humans and agents use the same resource protocol. Observation, control, and
   lifecycle ownership are independent. Revocation fences queued mutations,
   including after control is returned to the original holder.
5. Concurrent agents have distinct information and authority even when they share
   resources. Environment ordering and observation visibility remain explicit.
6. A disconnected viewer does not own the run. Reconnection has a durable cursor;
   retries of submissions cannot start duplicate work. Process restart has a
   separate, honest recovery contract for uncertain external effects.
7. The execution record preserves exact generation inputs, attempts, policy
   provenance, multimodal references, actions, observations, and interventions.
   Evaluation, SFT, and RL are different projections of that record.
8. The prelude is browsable versioned context material. Current documents and the
   versions actually consumed by a run are distinguishable.
9. Every abstraction has a concrete consumer or invariant. Actor scheduling,
   training algorithms, database choice, transport, and provider protocol can vary
   without redefining the agent's meaning.

The evidence and scope of verification are recorded in `VALIDATION.md`.

## Candidate boundaries

The strongest current candidate has three independently useful parts:

- **Data:** immutable inputs and outputs, context transformations, and execution
  records. Consumers include a context inspector and offline dataset exporters.
- **Controller:** a deterministic transition over run state. It describes effects;
  an interpreter performs them. Consumers include interactive and batch runners.
- **Resource protocol:** identities, observations, operations, and control grants.
  Consumers include human clients, agent tool adapters, and simulated environments.

These are dependency boundaries. The experiment uses modules, without committing
to three published crates or introducing a general actor framework.

The runtime composes them. Sessions, service discovery, CLI/web frontends, and
training workers are consumers. Model adapters translate provider protocols;
resource adapters translate environment operations. A tool's model-facing schema
is one presentation of an operation, rather than the identity of a resource.

## Run the experiment

The checks are deliberately adversarial: cancellation at each boundary,
reverse tool completion order, control takeover and return, context compaction
with a live resource, independent agents sharing that resource, journal failure,
late model results, incomplete token evidence, and training export after an
intervention. See `SOURCES.md` for external evidence and its limits.

From this directory:

```sh
cargo test --locked --offline
cargo clippy --locked --offline --all-targets -- -D warnings
cargo fmt --check
cargo run --locked --offline --example rollout
```

The example prints two generation records from a simulated policy and counter.
The model input comes exclusively from the recorded context. There is no API
request, shell execution, or model training in the example. A captured output is
included as `example-rollout.json` for inspection.

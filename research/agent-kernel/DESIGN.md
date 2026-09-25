# A small foundation for agent execution

Status: proposed contracts, supported by the semantic experiment in this directory.
The target is a reusable library; Myco is one application of it. External evidence
and local reference implementations are discussed in [SOURCES.md](SOURCES.md).

## The decision

Use immutable values for knowledge and records, a pure controller for decisions,
and explicit interpreters for effects. Use actors when an interpreter needs
concurrent ownership of a run or resource. Keep dataset readers independent of
both the controller and the actor runtime.

Three boundaries earn their place:

| Boundary | Minimal responsibility | Independent consumer |
| --- | --- | --- |
| Data | Preserve inputs, outputs, provenance, and causal links | Context browser; evaluation/SFT/RL exporter |
| Controller | Advance run state in response to an input, describing effects | Scripted evaluator; interactive runner |
| Resource protocol | Observe state and apply authorized operations | Human client; agent tool adapter; environment simulator |

The same data boundary should support instrumenting an existing ordinary async
agent. Requiring every training producer to adopt our controller would defeat
decoupling. Conversely, the controller should work with in-memory values and a
simple interpreter without a database, server, or session.

Start with these as modules. Extract published crates only when independent
consumers exist. Model adapters and training exporters are replaceable leaves.
No universal plugin registry, transport framework, or actor trait is needed to
state the contracts.

## Values and lifetimes

| Concept | Definition and lifetime |
| --- | --- |
| Content | Text or immutable typed bytes. Large payloads are blob references. |
| Message | A model-facing communication with semantic role, source attribution, and content. Tool observations retain their call link. |
| Context | An immutable ordered sequence of messages. A growing conversation creates revisions of one lineage. |
| Model input | Context plus instruction/material snapshots, tool definitions, generation settings, and adapter-specific input. |
| Generation | One actual inference attempt against a frozen model input. Retries get new attempt identities. |
| Operation | Identified external work: generation, tool execution, or another capability invocation. Request, outcome, and uncertainty have separate records. |
| Run | One evolving controller state, including its current context and outstanding operations. A run may span compactions. |
| Principal | The authenticated actor responsible for an operation. Its identity is independent of run, context, or client connection identity. |
| Resource | Live environment state with its own identity, incarnation, observation revision, and lifecycle owner. |
| Session | Application grouping of context lineages, runs, metadata, and resource access. |
| Episode | An environment-defined interaction interval, potentially involving several agents and runs. |
| Dataset example | A selected projection of execution records under an explicit export/grading policy. |

Agent behavior is a controller plus the capabilities it receives. An agent
identity can persist across runs. A separate mutable `Agent` object owning model,
conversation, tools, storage, and renderer is unnecessary.

"Turn" is a presentation grouping: for Myco it can be an accepted human request
and the work responding to it. The kernel uses generation and operation
boundaries because autonomous agents and simultaneous environments need not have
human turns. An application can call a context lineage a Thread; a second
Timeline abstraction adds no capability.

Authors and model roles are separate. A message sent by another agent may be
presented as external input; that principal is not automatically the model's
assistant role. Tool output does not acquire instruction authority from being
stored in the same record format.

## Context is knowledge; the environment is elsewhere

`append(context, messages)` returns a new revision. Existing readers keep the
old value. A generation pins all of its input, including prelude and tool-schema
versions, before inference begins.

`derive(source, replacement_messages, provenance)` creates a new lineage for a
compaction, fork, or context repair. The source remains unchanged. Provenance
identifies the source revision, transformation recipe, and producing operation;
a summary is an observation produced by a summarizer, not a claim that omitted
events never happened. A retained tool call must retain its matching results.
The application commits a new current-context pointer with compare-and-swap
against the source revision. A late summary cannot replace newer input.

The compactor is an ordinary bounded job using a model capability. A controller
can yield at a complete tool boundary, run the compactor, install the derived
context, and continue the same run. Compaction failure keeps the source usable.
The kernel has no fixed token threshold or summary prompt. The experiment tests
installation boundaries; it does not implement a summarization policy.

A context fork and an environment fork are separate requests. The environment
adapter declares whether it can reset, clone, checkpoint, or only share its
state. A live shell generally cannot be cloned by copying a transcript. For
controlled evaluations, a fresh environment is an explicit fixture operation.

Context is a partial observation of the world. Treating it as a complete Markov
state would silently assume away hidden files, other actors, and information lost
by compaction. Training adapters choose the observation/state interpretation
required by their algorithm.

## The controller

The useful functional boundary is:

```text
advance : State × Input → Result<(State, Effects), Rejection>
```

State contains the current context reference, execution phase, pending operation
IDs, queued input, and policy state such as budgets. Inputs contain external facts
and explicit requests. Effects describe external work. Time, random seeds, IDs
that require randomness, and policy updates must be explicit inputs or captured
effects if replay depends on them.

The experiment provides one conventional model/tool controller. It samples,
dispatches a tool batch, gathers results, and reaches another generation boundary.
That policy is replaceable. The function signature alone needs no trait; a second
implementation should justify any generic interface.

Five rules prevent accidental coupling:

1. The controller never awaits a model or tool. The interpreter delivers outcomes.
2. The identity of an issued operation survives retries of message delivery. A
   fresh inference attempt receives a fresh identity, even with identical input.
3. Tool results are recorded on arrival. A context projection can order a batch
   by call position to satisfy the model protocol without rewriting arrival order.
4. Cancellation is a request. Tool outcomes still distinguish success, failure,
   acknowledged cancellation, and unknown effect. A stopped viewer is not a stop
   request. A model answer is not evidence of task success.
5. Interjection during generation can supersede it. Late responses remain facts
   but cannot produce new actions. During tools, the supplied controller queues
   interjection, requests cancellation, and settles results before generating.

Streaming is operation progress. Each chunk carries an operation ID and sequence;
the view ignores progress from superseded operations. The completed response or
failed partial attempt is recorded once. An adapter must complete and validate a
tool request before it can cause an effect. Chunk-by-chunk streaming and provider
assembly are deliberately outside this semantic experiment.

Retry/backoff and compaction are controller policies. Timers are described
effects with delivered expiry inputs. Policy selection is separate from whether
the interpreter happens to run locally, in a service, or on a rollout worker.

## Actors, sessions, and service execution

A run actor owns the mutable pointer to run state and its mailbox. A resource
actor owns live resource state. Both can use the same pure transitions that a
single-threaded evaluator drives directly. Long work executes outside their
mailboxes and reports completion. Mailbox ownership supplies local serialization;
it supplies neither durable storage nor external-effect atomicity.

A session record contains references and metadata. A session runtime supervises
run handles and resource scopes. Compaction, an agent finishing a response, and a
client disconnecting do not destroy the scope. Closing the scope applies an
explicit resource cleanup policy. Across restart, durable references become
unavailable or newly attached resources with fresh incarnation IDs until proven
otherwise.

The persistent and private service share three operations:

```text
submit(command_id, target, command) → committed receipt
observe(target, cursor)           → snapshot / committed records / new cursor
cancel(command_id, operation)     → committed cancellation request
```

`command_id` identifies a logical submission across retries. The server commits
the receipt and command together; reusing the ID with different content fails.
One writer owns each run, including across processes. A viewer's cursor is its
own read position and never consumes events for another viewer. Slow viewers
resynchronize from durable records; a lossy UI broadcast is only a wake-up hint.

For Myco, discovery may select a persistent per-user/profile service, with a
private service when absent. Handshake incompatibility and an unhealthy existing
owner are explicit failures, not evidence of absence. A private server's lifetime
must be declared: it can stay alive with work after the spawning client exits,
then retire when idle. Networking and installation are application adapters.

The commit boundary is: decide → persist the input, decision, and pending effects
atomically → publish effects → record outcomes. Storage failure suspends new
effects. After a crash between publication and outcome recording, reconcile with
the resource using the operation ID when supported. Otherwise record unknown
effect and require an explicit recovery decision. Never infer "not executed"
from a missing reply. Durable execution cannot make arbitrary shell side effects
exactly-once.

Operation recovery capabilities can remain a small enum at the interpreter:
`queryable`, `idempotent_with_key`, or `uncertain_on_disconnect`. Adapters must
earn stronger guarantees. Model retries also have cost/provenance implications
even when they cannot mutate a filesystem.

Bounded worker queues and admission budgets belong to the scheduler. Keep control
messages responsive under progress traffic; use independent bounded progress
delivery or coalesced views. Long-lived resource work must have supervised cleanup
and cancellation acknowledgement. Those liveness mechanisms need runtime tests
when the actual async runner is built.

## Shared resources and the RwLock analogy

Three questions have different answers: who may observe, who may mutate, and who
owns lifetime. Represent those policies separately. A read can return an immutable
snapshot `(resource incarnation, observation revision, content)` without granting
mutation. Every observer has its own cursor for streams such as terminal output.

Control is a visible grant:

```text
Grant = (resource incarnation, authority epoch, principal)
Mutation = (operation id, verb, arguments, grant, optional expected revision)
```

The arbiter authorizes transfer according to application policy. Myco can allow
human takeover of an agent's resource; an evaluation can use a different policy.
Every transfer increments the epoch, including release and reacquisition. The
resource checks the authenticated principal, incarnation, and epoch immediately
before beginning a mutation, serialized with transfer. Returning control to the
same principal never revalidates old queued commands.

The optional expected observation revision addresses another problem: a caller
may retain valid control but have stale knowledge because a process or another
authorized mechanism changed the resource. Authority fencing and optimistic
read-modify-write validation are independent checks.

This is RwLock-like authority, with snapshot reads and an exclusive controller.
Actual atomic reads/writes inside a resource can use a short mutex, RwLock, or
mailbox. Holding a memory lock throughout model inference would give the wrong
lifetime and obstruct human interaction. Some reads require live I/O; they may
wait on that resource, and are not promised immediate or parallel execution.

Takeover fences work that has not begun. A command already running may keep
changing the world until a resource-specific cancel completes. Shells sharing a
filesystem are distinct resources but alias the same world; exclusive terminal
control does not provide filesystem transactions. Stronger coordination must
name the actual shared resource. These limits belong in the UI's meaning of
"take control."

## Swarms and environments

Compose multiple controllers with distinct contexts, stable principals, and run
IDs. A scheduler routes their effects and observations. A parent run can call a
spawn capability; the child gets explicit input, capability bindings, and a
resource scope. The child's answer arrives as an attributed observation carrying
causal links. A child run is independently inspectable and can outlive an
individual parent generation according to supervision policy.

No single global conversation needs to contain every agent's messages. Delivery
to a particular agent creates the observation it actually sees. A trace links
cross-agent sends and receives; per-owner sequence numbers define local order.
Wall-clock timestamps aid inspection but do not prove causality.

Simultaneous games need a barrier that collects a joint action before stepping
the environment. Asynchronous coding agents need independently dispatched work.
Both can interpret the same operation records with different schedulers. Neither
the controller nor a resource's authority mechanism should silently choose the
environment's stepping semantics. A global synchronous step API and a universal
swarm object are therefore deferred.

## Data suitable for evaluation and training

The durable interchange is an execution record, independent of controller
checkpoints. A checkpoint helps one runner resume. A generation/action record
helps arbitrary tools understand what happened. Exporters must not have to load
old controller code merely to read training examples.

The first production schema should have these logical records:

| Record | Required facts |
| --- | --- |
| Run description | Run and principal IDs; parent/episode links; controller/config revisions; task input and environment fixture identity |
| Model request | Generation/attempt ID; ordered context revision; exact instructions/material; tool schemas and binding revision; model request settings; serialized provider input or immutable reference |
| Model outcome | Request link; complete response or explicit failure with partial output; raw provider payload/reference where available; finish reason, usage, timing; applied/superseded disposition |
| Action request | Operation ID; originating generation and call position; authenticated caller; resolved resource incarnation/verb; arguments; grant and preconditions |
| Action outcome | Request link; observed output/artifacts and resource revision; status including unknown; timing; side-effect receipt when available |
| Context derivation | Source revision, destination revision, transform recipe, producing operation, retained-content references |
| Intervention | Human/system identity; delivery target; requested change; causal link; application/rejection result |
| Annotation | Target record IDs; evaluator/recipe version; reward, score, preference, or label; evidence references |

This is a compact versioned envelope plus typed bodies, not one enormous record
with every optional field. Stable IDs and body versions permit new optional data.
Missing values mean unavailable, not zero. Unknown extension payloads can be
preserved by readers without claiming they understand their semantics.

Immutable blobs deduplicate repeated prompts, images, audio, and tool artifacts.
Store the bytes and verify digests; an external mutable path is not a preserved
observation. A manifest orders message references, so deduplication cannot alter
the prompt. Commit reachable blobs before their referencing record. Export bundles
contain all referenced content or declare missing material and reduced fidelity.
The experiment inlines values for readability and tests blob-reference round trips;
it is not a content-addressed store implementation.

Exactness has levels: logical model input, actual provider request, and serving-side
token/tensor evidence. Preserve opaque provider blocks needed for continuation
without pretending they are portable supervised targets. Strip credentials from
captured request metadata. An exporter declares the fidelity it requires.

For **SFT**, select successful or corrected decisions under a versioned selection
policy; preserve the prompt, tools, assistant target, and target spans. Human
corrections require explicit promotion to demonstration targets. Tool output and
retrieved text remain conditioning input. After compaction, use the actual compacted
input for that generation. A summary generated by a different model is not silently
relabeled as an action by the target policy.

For **RL**, preserve actual serving-side prompt/completion tokens, served policy
revision, tokenizer/template revision, log probabilities and their sampling
convention, plus multimodal model inputs when relevant. Retokenized text cannot
establish these facts. Record the served policy per attempt because asynchronous
workers may straddle policy updates. Algorithms choose whether such samples are
usable. Store episode completion/truncation separately from run finish reasons.
Rewards and credit assignment are annotations, not edits to historical outputs.

For **evaluation**, associate tasks, seeds where meaningful, environment setup,
artifacts, outcomes, costs, interventions, and grader versions with the same
records. Deterministic controller replay tests software behavior. Real-model
evaluation measures task quality; deterministic seeds alone do not reproduce a
changing external environment. Grader access to hidden state must not leak into
the acting agent's context or supervised prompt.

Keep native traces and export dataset formats as versioned projections. A task can
produce multiple rollouts; a rollout can produce many generation examples; a
multi-agent episode can link multiple runs. Avoid assigning one session-level
reward indiscriminately to every message.

## Prelude browsing

The prelude needs no agent-specific storage primitive. It is a versioned collection
of named documents that context assembly snapshots. The browser can expose:

- list/filter entries and render their content;
- inspect source, revision, size, and current collection membership;
- compare versions and inspect the exact material used by a selected generation;
- edit through the same authorized document operations available to agents.

Collection membership and order have a version, as do individual entries. A model
input pins both. Editing an entry changes future assemblies; existing generations
retain the old bytes. Refresh of a live agent is explicit at a generation boundary,
so the UI can distinguish current documents from the agent's current input. This
also supports studying instruction changes in evaluations.

## Alternatives and the cost of this choice

| Candidate | Strength | Limit for this task | Verdict |
| --- | --- | --- | --- |
| One async agent owning model/tools/history | Very small direct implementation; easy sequential calls | Opaque pending futures, lifecycle ownership, and input mutation need extra machinery for reconnect and replay | Good adapter or simple consumer; insufficient as the sole data/runtime contract |
| Everything is an actor | Uniform message transport and local ownership | Immutable values gain unnecessary identity; actors alone do not specify data provenance or durability | Actors for live owners |
| General workflow graph / event engine | Arbitrary orchestration and visualization | Graph compilation, extensibility, migrations, and scheduling become prerequisites | Defer until concrete workflows require it |
| Environment `step(action)` as the entire core | Natural for a controlled sequential simulator | External observations, simultaneous actions, and live human intervention need scheduling semantics | Supply as one environment interpreter |
| Pure controller + explicit effects + independent records | Replayable decisions; interchangeable interpreters; datasets independent of Myco | More explicit state, operation identities, and controller-version discipline | Recommended foundation |

Purity does not mean cloning every historical byte on each transition. Structural
sharing and immutable references can preserve the model while reducing cost. Nor
does every helper deserve a public abstraction: keep response assembly inside a
model adapter, shell processes inside a resource adapter, and projections as
ordinary functions.

## Implementation order and remaining decisions

1. Stabilize data records and the context/operation identities using this semantic
   model and a read-only capture adapter for the existing agent.
2. Build a small interpreter with a real journal and one persistent shell resource.
   Exercise process crash, takeover, and two reconnecting clients before transport
   expansion. Define cursor retention and snapshot resynchronization.
3. Bind the existing model adapters, move model/tool work out of the owner mailbox,
   and keep a synchronous in-memory interpreter for evaluation fixtures.
4. Add context inspection and prelude browsing against the same immutable records.
   Build the CLI and web application over the service contract.
5. Add one SFT exporter and one trainable-backend rollout adapter against actual
   trainer fixtures. Prove token fidelity and environment isolation at that boundary.

These are implementation gates, not claims that the experiment ships those
features. Public Rust names, wire encoding, persistent-store selection, context
chunking, policy-update handling, and exact live-resource recovery capabilities
remain implementation choices. The semantic decisions above do not require
choosing them in advance.

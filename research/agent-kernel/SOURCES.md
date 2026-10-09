# Research evidence

Primary sources consulted September 19, 2026. The conclusions below are design
inferences, not claims that adopting a framework solves Myco's requirements.
Links to live documentation can change; the specific observed contracts are
summarized here. No external framework is a dependency of the experiment.

| Source | Relevant evidence | Consequence for this design |
| --- | --- | --- |
| [Elm commands and subscriptions](https://guide.elm-lang.org/effects/) | Updates describe commands and subscriptions; the runtime executes them and supplies messages. | A deterministic controller can describe model/tool work without owning I/O. Streaming and external observations arrive as inputs. |
| [Erlang process semantics](https://www.erlang.org/doc/system/ref_man_processes.html#delivery-of-signals) | Ordering is between one sender and one receiver; distributed signals may be lost. | Actor scheduling does not supply global causality, persistence, or exactly-once effects. Record operation identity and per-owner order explicitly. |
| [Temporal activities](https://docs.temporal.io/activities) | Activities can be nondeterministic; idempotence is recommended because attempts may repeat. | Keep effect interpretation separate. Classify recovery by operation capability; arbitrary shell commands cannot receive a blanket retry guarantee. |
| [Chubby, section 2.4](https://www.usenix.org/legacy/events/osdi06/tech/full_papers/burrows/burrows.pdf) | A sequencer includes lock identity, mode, and generation; the receiving server checks it. | Revocation must be checked where a protected mutation starts. Checking only the caller identity fails after takeover and return. |
| [PettingZoo AEC](https://pettingzoo.farama.org/api/aec/) and [parallel API](https://pettingzoo.farama.org/api/parallel/) | Sequential and simultaneous multi-agent environments expose different stepping contracts. | A swarm is a composition of local controllers and an explicit environment scheduler. Do not force every environment into globally alternating user/assistant turns. |
| [Agent Lightning v1.0 basics](https://microsoft.github.io/agent-lightning/stable/05-basics/) | Model requests and other events belong to a rollout; rollout controller, model gateway, and trainer have separate responsibilities. A rollout differs from a training example. | Capture each generation at the inference boundary. Keep training algorithms outside the execution controller. Model-call capture alone is insufficient for tool-effect auditing. |
| [vLLM/Agent Lightning token identity](https://vllm.ai/blog/2025-10-22-agent-lightning) | Decoding then re-tokenizing can change token identity; the serving interface can return actual prompt and completion token IDs. | Preserve serving-side token evidence when available. Text plus a tokenizer name cannot reconstruct the sampled token sequence reliably. |
| [TRL SFT trainer](https://huggingface.co/docs/trl/main/en/sft_trainer) | Completion-only and assistant-only loss require explicit boundaries and, for assistant masks, template support. | Store attribution and examples before flattening; exporters choose targets and masks. Human intervention and tool observations must not become accidental assistant targets. |
| [TRL GRPO trainer](https://huggingface.co/docs/trl/main/en/grpo_trainer) | The custom rollout interface requests prompt IDs, completion IDs, and log probabilities; environment factories reset environments for rollouts. | Training capability is explicit. Keep policy revision, tokenization, log-probability convention, and environment reset/clone capability in the contract. |
| [RLDS format](https://github.com/google-research/rlds#dataset-format) | Episodes and steps have separate identities and metadata; terminal and truncated endings differ. | Session, run, dataset example, and environment episode are distinct. A model answer alone does not establish environment termination or task success. RLDS is precedent, not a proposed dependency (repository archived). |
| [Inspect evaluation logs](https://inspect.aisi.org.uk/eval-logs.html) and [intervention](https://inspect.aisi.org.uk/intervention.html) | Logs distinguish messages/events, deduplicate attachments, and preserve operator interventions. | Maintain a browsable view and a richer execution record. Record human actions with their own identity and preserve artifact references. |

## Local evidence

Myco `acf6520` has independent model and headless agent crates, session/thread
storage, a session runtime owning tools, and a prelude store. Its generation
controller still directly awaits providers and tools; presentation events are
not a complete durable execution record. The existing boundaries are useful
implementation references, not constraints on this experiment.

`main-v3` at `5f193fa` demonstrates per-resource actors, a driver seat, immutable
observations in chat, and asynchronous turns. Its instance dispatcher rechecks
the current principal at application time, without a grant generation. The
takeover-and-return counterexample is reproduced by `v3-takeover-repro.patch`;
see `VALIDATION.md`. Its pool is RAM-only. Its default model tools do not expose
control of an existing terminal. These are reasons to evaluate individual
contracts independently.

Repository references: `crates/myco-agent/src/lib.rs`,
`crates/myco-agent/src/generation.rs`, `src/session_runtime.rs`,
`src/session/thread.rs`, `src/prelude.rs`; v3 `crates/instance/src/lib.rs`,
`crates/kind-chat/src/tools.rs`, and `DESIGN.md`.

# Validation and completion audit

September 19, 2026. Scope: research and develop reusable abstractions, then test
their semantics. This is a design deliverable and executable reference model.
It is not a production service, model adapter, database, or training framework.

## Evidence by requirement

The numbers correspond to `README.md`.

| Requirement | Authoritative evidence | What it establishes / limit |
| --- | --- | --- |
| 1. Independent headless execution | `src/run.rs`; `Cargo.toml`; `actor_and_direct_interpreters_produce_identical_semantic_records`; `examples/rollout.rs` | The controller has no I/O, actor, Myco, or session dependency. Direct and mailbox interpreters agree. Real model adapters are a separate integration gate. |
| 2. Context and resource lifetimes | `compaction_keeps_old_observations_and_does_not_own_live_resources`; `compaction_cannot_install_against_another_revision_or_during_effects`; `a_context_projection_cannot_orphan_or_duplicate_tool_results` | Old observations remain fixed; new context installation rejects stale revisions and incomplete tool boundaries. The resource fixture is a counter, not a persistent OS shell. |
| 3. Async control and late outcomes | `an_actor_stays_responsive_while_model_work_and_viewers_are_detached`; cancellation/interjection/retry tests in `tests/scenarios.rs` | Pending work leaves the mailbox responsive; late results and duplicates do not redispatch. Cancellation waits for tool outcomes instead of inventing cancellation success. No real provider streaming/cancellation is claimed. |
| 4. Human/agent control | `src/control.rs`; `takeover_and_return_does_not_revalidate_old_commands`; `every_authority_epoch_invalidates_all_previous_grants`; `control_and_observation_revisions_protect_different_invariants` | Incarnation and grant epoch fence stale authority, including 243 five-transfer histories. Observation freshness is a separate precondition. The surrounding authorization policy and atomic mutation owner are adapter responsibilities. |
| 5. Swarm composition | `two_agents_share_a_resource_without_sharing_context_or_operation_identity`; `the_same_agent_can_run_again_without_reacquiring_its_live_resource`; `tests/environment.rs` | Independent controllers share resources with distinct contexts; principals survive run changes. A joint-action fixture demonstrates simultaneous stepping without merging private observations. No distributed swarm scheduler is implemented. |
| 6. Service/recovery contracts | `tests/runtime.rs`; DESIGN sections on service execution and recovery | Submission receipts, independent read cursors, commit-before-publish, and unknown-effect reconciliation work in an atomic in-memory store model. HTTP, discovery/fallback, fsync, real crash injection, and cross-process writer enforcement are specified design gates, not tested implementations. |
| 7. Data for eval/SFT/RL | `GenerationRecord` and `Sample` in `src/data.rs`; JSON roundtrip, token-evidence, output-limit, independent-record, and label-isolation tests; `example-rollout.json` | Exact logical inputs and outcomes are preserved; exporters can read independent records; failed/late/truncated attempts remain inspectable; absent token evidence fails explicitly. No trainer has consumed the prototype and no learning-quality result is claimed. |
| 8. Prelude browsing | `Document` in `src/data.rs`; `current_prelude_edits_do_not_change_historical_model_inputs`; DESIGN's browsing contract | Current and historical material can be distinguished without changing old model inputs. The list/filter/diff/edit UI and a document store are proposed consumers, not implemented UI. |
| 9. Minimality and independence | DESIGN boundary/alternative tables; dependency tree; module imports | Each boundary has an independent consumer and a counterexample supporting it. Serialization is the only external dependency family. Actor, transport, storage, and trainer choices remain outside the kernel. This is a design argument, not a performance benchmark. |

## Commands and results

Run from this directory:

```sh
cargo test --locked --offline
cargo clippy --locked --offline --all-targets -- -D warnings
cargo fmt --check
cargo run --locked --offline --example rollout
```

The suite contains 30 passing tests: 22 controller/data/resource scenarios,
6 runtime/interchange scenarios, and 2 environment/export scenarios. No tests
are ignored. The handoff test enumerates 243 schedules within one test. Clippy
with warnings denied and formatting checks pass. The example emits two complete
generation records and ends with counter value 1. Its second model input contains
the first call and observation; its answer is derived from that observation.
`example-rollout.json` is the captured output, with serving token evidence absent
because the policy is simulated.

The production Myco workspace is unchanged by these files. The experiment's
nested `[workspace]` keeps it out of the production build. The v3 reference
checkout was restored after the counterexample below.

## Counterexample against main-v3

Tested `main-v3` at `5f193faac057f72137179528c6259bc971688f44` using the saved
`v3-takeover-repro.patch`. It adds one test to the existing counter fixture:

1. Agent owns the counter; poll its increment request until it is queued.
2. Human takes control, then releases it to the creator agent.
3. Let the queued request reach the resource mailbox.
4. Require the old command to be fenced.

The current-thread test uses explicit polling, without sleeps or a timing race.
The test fails on v3 with:

```text
old command executed after takeover and return: Ok(Number(1))
```

Reproduce in a clean disposable checkout of that commit:

```sh
git apply --unidiff-zero /absolute/path/to/v3-takeover-repro.patch
cargo test --locked --offline -p myco-instance readiness_takeover_and_return -- --nocapture
git apply --unidiff-zero -R /absolute/path/to/v3-takeover-repro.patch
```

Failure is expected. This verifies why principal equality alone cannot enforce
the documented takeover fence. The new experiment rejects the old grant after
the same handoff. No fix has been applied to the production v3 branch.

## Review findings incorporated into the design

- **Training readers depended on controller replay.** Added a standalone
  generation record and an independent reader test. The controller journal is
  an implementation detail; it can export the common record.
- **Agent identity was initially derived from run identity.** The constructor now
  requires a principal explicitly, and a test covers one agent across two runs.
- **Authority could be mistaken for fresh knowledge.** Added the independent
  expected-revision contract and its resource fixture test.
- **Transport success could be mistaken for complete model output.** Added an
  explicit output-limit finish and a test retaining the partial attempt while
  refusing to execute its calls or mark the run answered. Continuation policy is
  deliberately left to a fuller controller.
- **A simulated policy could accidentally read live environment state.** The
  example's answer now reads only its supplied context. The joint-environment
  tests separately check private-observation and hidden-grader isolation.

## Deliberate limits

The experiment uses trusted in-process values, caller-supplied identities,
inlined snapshots, and an inefficient replay-on-each-append test journal. It does
not authenticate principals, implement content-addressed storage, model every
provider's content blocks, persist grant epochs, handle real process reattachment,
enforce resource ACLs, or implement streaming/backoff/compaction policies. Its
`Finish` enum only distinguishes complete output from an output limit; real
adapters must preserve richer native reasons. These choices keep the semantic
model small and inspectable; they are not proposed production shortcuts.

The actual implementation gates in DESIGN remain necessary before claiming a
deployable library or service. The research conclusion is explicit: the proposed
boundaries cover every requested use case, have been
compared with alternatives and primary sources, and have executable evidence for
the selected cross-cutting invariants and failure cases.

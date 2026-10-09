# Evaluations

`myco-eval` freezes task cases from existing sessions or new prompts, then runs
them through the same `SessionRunner` as the server. Each attempt uses a fresh
workspace and profile. An independent grader checks the resulting artifacts.

```bash
myco-eval from-session SESSION_ID /private/evals/fix-parser \
  --user-message 12 --repo /path/to/repo --revision STARTING_COMMIT \
  --grader grader.py --split test
myco-eval run /private/evals --config /path/to/config.toml \
  --model MODEL_KEY --output /private/results --repeat 3 \
  --max-requests 40 --timeout-secs 600
myco-eval report /private/results --min-success-rate 0.8
```

Choose the starting revision explicitly: a conversation is not a filesystem
snapshot. Export only the prefix before the task's answer, and review earlier
context for answer leakage. Keep session exports and results private unless
reviewed for publication.

Reports distinguish full task success, partial artifact quality, timeout, and
infrastructure failure. They retain request/token counts, tool observations,
latency, and cost estimates. Repetitions and held-out tasks help measure whether
a model reliably completes your work.

Run artifacts include versioned `provenance.json` with effective settings and
the main-agent prompt, plus timestamped `events.jsonl` with request/tool identity
and failed or abandoned attempts. Authentication values are excluded from the
configuration snapshot. Git runs include a self-contained pinned source bundle
and a relative frozen recipe; they can replay after moving the run and removing
the original repository, using separately supplied configuration and tools.
The [task eval manual](../manual/evals.md) documents
artifact portability, event fields, and fingerprint reuse.

`cargo test --locked --offline --test evals` exercises the executable against
local scripted providers: isolated fixtures, cache invalidation after policy
changes, grader failures, request limits, broken streams, transient recovery
without repeating completed tool effects, and deadline cancellation during
backoff. These exact runtime checks need no model credentials; they do not
measure real-model completion across repeated compactions.

The optional GEPA adapter optimizes the prelude using bounded Myco task and
reflection runs. It separates train/validation/test cases and defaults to
explicit free OpenRouter models. It requires no additional dependency in the
Rust CLI. See the [task eval manual](../manual/evals.md) for case formats,
graders, spending guards, resume behavior, and the GEPA command.

For a custom embedding, use [`SessionRunner`](../api/myco/chat/struct.SessionRunner.html)
or the lower-level [agent API](agents.md).

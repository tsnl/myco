# Evaluations

`myco-eval` freezes task cases from existing sessions or new prompts, then runs
them through the same `SessionRunner` as the CLI. Each attempt uses a fresh
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

The optional GEPA adapter optimizes the prelude using bounded Myco task and
reflection runs. It separates train/validation/test cases and defaults to
explicit free OpenRouter models. It requires no additional dependency in the
Rust CLI. See the [task eval manual](../manual/evals.md) for case formats,
graders, spending guards, resume behavior, and the GEPA command.

For a custom embedding, use [`SessionRunner`](../api/myco/chat/struct.SessionRunner.html)
or the lower-level [agent API](agents.md).

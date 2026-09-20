# Task evals and prelude optimization

`myco-eval` creates frozen task cases and runs them through the same
`SessionRunner` as `myco`. It is a separate process: run it from a terminal,
cron, or a service without leaving an interactive conversation open.

## Build a case from a session

An agent can use `session_meta list` or `list_recent` to find sessions, then
`session_history range` to locate the human request to replay. Choose its
**zero-based message index**, before the answer or tool actions that solve it.
Write an independent Python grader before exporting:

```bash
myco-eval --profile default from-session SESSION_ID /private/evals/fix-parser \
  --user-message 12 --thread THREAD_ID \
  --repo /path/to/repo --revision COMMIT_BEFORE_THE_FIX \
  --grader /private/grader.py --split test
```

The command copies that thread's prefix through the chosen request, referenced
images, the grader, and the pinned workspace recipe. It excludes later answers,
other threads, and session metadata. Source sessions are not modified. Inspect
`context.json` before adding a case to a dataset: earlier context or compaction
summaries may already contain a solution. Source history is private data; keep
cases and results outside public repositories unless reviewed for publication.

A session is conversation context, **not a filesystem snapshot**. Pick the
starting commit explicitly. Earlier shell handles and editor state are not
restored. Adapt tasks that depend on external services or unavailable files.
Git workspaces require the local repository and commit to remain available;
submodules, dependency installation, and external fixtures need preparation.

For a new task, or a small directory snapshot:

```bash
myco-eval create /private/evals/example --task-file task.md \
  --workspace ./fixture --grader grader.py --split train
```

Fixture copying excludes `.git`, `.myco`, `target`, and `__pycache__`, and rejects
symlinks. Runs initialize a fresh Git repository around the copied fixture so
project guidance from the eval runner's parent directories is not inherited.
Use a pinned Git case for source trees with symlinks.

## Grade actual work

A case is a directory containing `case.json`, `context.json`, `grader.py`, and
optional `workspace/` and `images/`. Manifest version 1 specifies the name,
split (`train`, `validation`, or `test`), workspace recipe, and grader argv.
The default command is `python3 {case}/grader.py`; edit `grader` in `case.json`
to use another program. Arguments are passed directly, without a shell.

The grader runs in the completed workspace. `MYCO_EVAL_WORKSPACE` gives that
path and `MYCO_EVAL_RUN` points to the run artifacts (including `agent.json`).
It must exit 0 and print exactly one JSON object:

```json
{"score": 0.75, "feedback": "Three of four checks pass; the empty input case fails."}
```

Scores range from 0 to 1. Test artifacts, repository tests, or explicit task
criteria; do not score the assistant's claim that it succeeded. Missing expected
artifacts should produce score 0 with feedback. A crashed, timed-out, or malformed
grader is an infrastructure error, recorded separately from an agent failure.
The frozen inputs and grader are checked for changes before and after grading.

## Run and compare

```bash
myco-eval run /private/evals --config ~/.myco/profiles/openrouter-free/config.toml \
  --model ling-free --free-only --output /private/results/baseline \
  --prelude candidate.md --repeat 3 --jobs 1 \
  --max-requests 40 --timeout-secs 600 --grader-timeout-secs 60
myco-eval report /private/results/baseline --min-success-rate 0.8
```

`--model` can be repeated for comparisons. `--split` selects a dataset split.
`--free-only` accepts only explicit OpenRouter model IDs ending in `:free`, at
OpenRouter's API URL, with no router or paid-model fallback. Without it, the
selected configured models determine spending. Each worker has its own request
budget and deadline; multiply these by cases, models, and repetitions when
planning an unattended run. Limit concurrency to match provider rate limits.

Each attempt has a fresh workspace and profile. The supplied prelude is the
entire candidate; the user's profile prelude is not inherited or edited. Local
standard tools and session tools are available. Remote hosts and nested model
runs are not configured. Tools retain ordinary Myco computer access: these
workspaces and separate graders are organizational boundaries, not containment.
Use your own container/environment when needed.

Runs keep `job.json`, `result.json`, `agent.json`, `events.jsonl`, worker/grader
logs, the workspace, and a normal session store with image sidecars. Credentials
are read from the named config/auth sources and are not copied into artifacts.
Do not put credentials in gateway URLs. Full traces can still contain private
work data or secrets encountered by the evaluated task.

Re-running the same command reuses finished results whose case, model, prelude,
limits, repetition, and Myco build fingerprints match. Interrupted attempts are
retained and retried in fresh workspaces; a still-running worker prevents reuse.
Use a new output directory for fresh stochastic samples. Model aliases and
external services can change independently of these local fingerprints.

Reports separate cohorts with different tasks/settings and group by model,
prelude, and split. Success means a normally completed run with score 1. The
mean score reports artifact quality, including partial results. Infrastructure
errors are separate; a threshold check also fails when they are present.

Token counts sum **every reported request**, including retries and compaction;
`requests_without_usage` exposes missing reports. Tool counters describe the
main agent; `tool_errors` counts tool errors, while process exit codes are
recorded separately in the trace. Latency covers the agent run, including compaction, not workspace
setup or grading. Free-only runs report zero estimated model cost. Other costs
are unknown unless `report --prices prices.json` supplies USD per million tokens:

```json
{"model-key":{"input_per_million":1,"cached_input_per_million":0.1,"output_per_million":3}}
```

The estimate separates cached input from total input. Missing request usage
makes the estimate unknown. Provider billing, cache writes, and non-model
services can differ; preserve the original token counts alongside estimates.
Use repeated cases, held-out tasks, and your own success floor before selecting
models for long-running work. The included examples verify the infrastructure;
they are not a model leaderboard or evidence about long-task reliability.

## GEPA

The repository's `evals/gepa/myco_gepa.py` adapter optimizes one component:
`{"prelude": "..."}`. GEPA evaluates candidates with `myco-eval`, reflects on
training failures and tool traces, and selects candidates using a separate
validation set. Both task execution and reflection use bounded Myco runs and
are guarded by `--free-only` by default. The core CLI has no Python/GEPA dependency.

```bash
python3 -m venv .venv-gepa
.venv-gepa/bin/pip install -r evals/gepa/requirements.txt
.venv-gepa/bin/python evals/gepa/myco_gepa.py /private/evals \
  --binary /path/to/myco-eval \
  --config ~/.myco/profiles/openrouter-free/config.toml --model ling-free \
  --seed-prelude evals/seed-prelude.md --output /private/gepa-run \
  --max-metric-calls 12 --max-reflections 4 \
  --max-requests 40 --reflection-requests 8 --timeout-secs 600
```

The adapter requires separate train and validation cases and rejects the same
source session across splits. It does not run or reflect on `test` cases.
Review `best-prelude.md`, then run it against the held-out test split with
`myco-eval run --split test`. Keep graders fixed across candidates and group
related task variants into the same split. Validation helps select a prelude;
only fresh held-out tasks measure how well it generalizes. Usage, cost estimates,
and traces remain available for each candidate and reflection. The default
optimization score is task quality; costs are measured, not hidden in that score.

GEPA checkpoints live in the output directory. Reusing it resumes optimization
when inputs match; changed tasks/configuration, seed prelude, or binary require a fresh output
directory. The adapter caps reflection calls separately. Budget exhaustion or
infrastructure errors stop with diagnostic artifacts; they do not count as an
improved candidate. See the upstream [adapter interface](https://gepa-ai.github.io/gepa/guides/adapters/).

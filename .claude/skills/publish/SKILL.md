---
name: publish
description: >-
  Publish a new myco release to crates.io and GitHub via the Publish workflow
  (semver bump, cargo publish, GitHub Release). Use when the user asks to
  release, publish, bump version, ship to crates.io, create a release tag, or
  dry-run the publish action.
compatibility: Requires gh CLI authenticated to github.com/tsnl/myco; repo secrets CARGO_REGISTRY_TOKEN and PAT_TOKEN for real publishes.
metadata:
  author: myco
  version: "1.0"
---

# Publish myco

Ship a version of **myco** using GitHub Actions workflow **Publish**
(`.github/workflows/publish.yml`), which runs
`tsnl/semver-bump-and-cargo-publish@v1` then creates a GitHub Release.

Do **not** hand-edit `Cargo.toml` version or run `cargo publish` locally unless
the user explicitly overrides this skill.

## Preconditions (check before any real publish)

1. **Working tree clean** on the intended branch (default `main`), or user has
   accepted publishing a dirty/specific ref.
2. **CI green on the tip commit** you will publish. Branch-protection contexts
   look like `CI / Check Formatting`, `CI / Lint`, `CI / Test`. The Publish
   action waits on **check-run job names** (no workflow prefix):
   - `Check Formatting`
   - `Lint`
   - `Test`
   Do not start a real publish knowing they are failing.
3. **Secrets** (repo Settings → Secrets and variables → Actions):
   - `CARGO_REGISTRY_TOKEN` — crates.io API token
   - `PAT_TOKEN` — fine-grained PAT with **contents: write** (push version
     commit + tag) on this repo
4. **crates.io metadata**: root `Cargo.toml` must set `license` (currently
   `MIT`) and ship a matching `LICENSE` file. If either is missing, stop and
   fix before a real publish (or dry-run only).
5. **Branch protection**: `main` requires CI + one review for human merges;
   the publish bot uses `PAT_TOKEN` and may need admin/bypass if push fails —
   confirm with the user.

## How to run the workflow

Prefer `gh` from the repo root:

```bash
# Dry run (default) — no commit, tag, or crates.io publish
gh workflow run Publish \
  --ref main \
  -f branch=main \
  -f bump_type=patch \
  -f dry_run=true

# Real publish (patch | minor | major)
gh workflow run Publish \
  --ref main \
  -f branch=main \
  -f bump_type=patch \
  -f dry_run=false
```

Watch the run:

```bash
gh run list --workflow=Publish --limit 5
gh run watch   # or: gh run view <id> --log-failed
```

Inputs (must match the workflow):

| Input | Values | Notes |
|-------|--------|--------|
| `branch` | e.g. `main` | Branch to checkout and bump |
| `bump_type` | `patch` / `minor` / `major` | Semver |
| `dry_run` | `true` / `false` | Default in UI is dry-run |
| `release_notes` | markdown string | Optional; prepended to the GitHub Release body above the install boilerplate |

## Fallback: publishing without dispatch access (agent sessions)

Remote agent integrations can usually push to their `claude/*` session
branch but get **403 on POST /dispatches**, so `gh workflow run` and the
Actions UI are unavailable to them. `publish.yml` has a push trigger for
this case: on any `claude/**` branch, committing a change to
`.github/publish-request.json` runs the same publish job with parameters
from that file:

```json
{
  "request_id": "v0.2.1-dry-run-1",
  "branch": "main",
  "bump_type": "patch",
  "dry_run": "true",
  "notes_path": ".github/release-notes-v0.2.1.md"
}
```

- `branch` is what gets published (checked out, bumped, tagged) — the
  session branch only hosts the trigger.
- `notes_path` points at a committed markdown file used as the top of the
  GitHub Release body; write it before requesting a real publish.
- `request_id` is inert; change it to re-fire with otherwise-identical
  parameters.
- Same sequence as dispatch: push with `dry_run: "true"` first, then flip
  to `"false"` and push again.
- Remove the request file (and notes file, if desired) from the branch
  once the release is verified.

The semver action (v1.0.4+, pinned by SHA in publish.yml) waits for
check runs on the publish branch's checked-out HEAD — for `main`, the
checks its CI push run already produced — so no CI configuration is
needed on the request branch itself.

## Choose bump type

- **patch** — bugfixes, docs, CI, no public API change
- **minor** — new backwards-compatible features
- **major** — breaking CLI/API/config changes

Ask the user if unclear. Default suggestion for routine ship: **patch**.

## Recommended sequence

1. Confirm branch tip and `git status`.
2. Confirm CI green on that SHA (`gh run list` / commit checks).
3. **Always dry-run first** if secrets or license were recently changed, or if
   this is the first publish of the day.
4. On dry-run success, run again with `dry_run=false` and the agreed `bump_type`.
5. Verify:
   - New git tag and version commit on the branch
   - [crates.io/crates/myco](https://crates.io/crates/myco) shows the version
   - GitHub Release exists for the tag
6. Tell the user install lines:

```bash
cargo install myco --locked
# or pin the tag:
cargo install --git https://github.com/tsnl/myco --tag <tag> --locked
```

## What the workflow does (do not reimplement)

1. Checkout `branch` with `PAT_TOKEN`
2. Seed MiniLM weights (HF cache + `scripts/seed-minilm-weights.sh`) so the
   publish crate builds
3. Wait for check-runs named `Check Formatting`, `Lint`, `Test` (see
   `wait_for_checks` in the workflow; not the `CI / …` protection strings)
4. Bump version in `Cargo.toml` / lockfile, commit, tag
5. `cargo publish` when not dry-run and on main
6. Create GitHub Release notes (when published)

Source of truth: `.github/workflows/publish.yml`.

## Failure modes

| Symptom | What to check |
|---------|----------------|
| Wait for checks timeout | CI not running or names mismatch; open the failing commit checks |
| crates.io reject / license | Add `license` + LICENSE file; dry-run still validates much of the path |
| Push rejected | `PAT_TOKEN` scopes; branch protection |
| Secrets missing | Workflow fails early — add `CARGO_REGISTRY_TOKEN` and `PAT_TOKEN` |
| Dry-run OK, real publish fails | Read publish job log; action may roll back version commit on crates.io failure |

## Out of scope

- Attaching multi-platform binary assets (not in the workflow yet; document
  source/install only).
- Publishing `crates/myco-gui` (not part of the root package publish).
- Live LLM integration tests (ignored in CI; unrelated to release).

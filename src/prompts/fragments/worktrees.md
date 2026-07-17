# Git worktrees for new features

**Default:** implement new features in a dedicated git worktree + branch, not on the user's current
checkout branch.

## When this applies

- New features, non-trivial refactors, multi-file experiments, or anything that should be reviewable
  as an isolated branch.
- Skip only when the user explicitly asks to edit the current branch/worktree, or the change is a
  tiny one-liner/docs fix they clearly want in place.

## Layout (repo being operated on)

Create worktrees **under that repo's** `.myco/worktrees/` (cwd's git root, not a global cache):

```text
{git-root}/.myco/worktrees/{branch-slug}/
```

Examples:

```bash
# From the repo root the user is working in:
git rev-parse --show-toplevel   # → GIT_ROOT
BRANCH="feat/short-description"
DIR="$GIT_ROOT/.myco/worktrees/$BRANCH"
mkdir -p "$GIT_ROOT/.myco/worktrees"
git worktree add -b "$BRANCH" "$DIR" HEAD
# Do all feature work with cwd / tool paths under $DIR
```

- Branch names: short, descriptive (`feat/…`, `fix/…`). Avoid reusing an existing branch name.
- Ensure `.myco/worktrees/` is gitignored in that repo when you create it (add an ignore rule if
  missing) so worktree checkouts are not committed as ordinary files.
- One feature ↔ one worktree/branch unless the user asks otherwise.

## Workflow

1. Create branch + worktree under `.myco/worktrees/` as above.
2. Implement and test **inside the worktree path** (edits, builds, commits).
3. Keep commits focused; prefer the worktree's branch for all feature commits.
4. Tell the user the branch name, worktree path, and how to review/merge (e.g. `git -C <path> log`,
   PR from that branch). Do not delete the worktree or force-push unless asked.
5. If the harness cwd is still the main checkout, use absolute paths into the worktree (or
   `git -C`) so you never mix feature edits into the main tree by mistake.
6. Register the worktree on the session with `session_meta` `add_link` (host + absolute path + branch).

## Cleanup

Only remove a worktree when the user asks, after merge/abandon:

```bash
git worktree remove .myco/worktrees/{branch-slug}
git branch -d {branch}   # or delete remote branch if they want
```

# Git worktrees for new features

**Default:** implement new features in a dedicated git worktree + branch, not on the user's current
checkout branch. Skip only when the user asks to edit the current checkout, or the change is a tiny
one-liner / docs fix they clearly want in place.

Create it **under the repo being operated on** (cwd's git root, not a global cache):

```text
{git-root}/.myco/worktrees/{branch-slug}/
```

- Ensure `.myco/worktrees/` is gitignored in that repo when you create it (add the rule if it is
  missing) so worktree checkouts are never committed as ordinary files.
- **Branch names follow the project's convention** — its `AGENTS.md` / `CLAUDE.md`, or the names
  already on its branches. Absent one, short and descriptive. Do not reuse an existing name.
- One feature ↔ one worktree/branch unless the user asks otherwise.
- Work **inside the worktree path**: absolute paths or `git -C` while the harness cwd is still the
  main checkout, so feature edits never land in the main tree by mistake.
- Register the worktree on the session with `session_meta` `add_link` (host + absolute path +
  branch) so a later agent can find it.
- Report the branch name and worktree path when you are done. Do not delete a worktree or
  force-push unless asked.

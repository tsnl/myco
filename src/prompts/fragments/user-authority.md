# User authority & privileged operations

**Never** use admin, elevated, or privileged capabilities to override the user's control of their
repos, reviews, or workflows — even when those capabilities are available on the account or host.

## Hard rules

1. **Do not land / merge PRs without explicit user approval** for that merge (or an equally clear
   standing instruction in this conversation that covers it). Opening a draft or ready PR,
   requesting review, and reporting status are fine; **merging is not**.
2. **Do not force-merge, admin-merge, or bypass required checks / branch protection**
   (`gh pr merge --admin`, `--merge` with admin override, skipping CI, dismissing required reviews,
   etc.) unless the user **explicitly** asked for that override in this conversation.
3. **Do not force-push, rewrite shared history, or delete branches/tags** on remotes the user cares
   about unless they explicitly asked.
4. **Do not disable, skip, or weaken hooks, required status checks, CODEOWNERS, or other guardrails**
   to "just get it in" (e.g. `--no-verify` on shared branches, editing protection rules) unless the
   user explicitly asked.
5. **Capability ≠ permission.** Repo admin, `sudo`, org owner, or a green "you could merge" button
   is **not** approval. If a normal path is blocked (failing checks, pending reviews, missing
   rights), **stop, explain the blocker, and ask** — do not route around it with privilege.

## Related admin-only / workflow-breaking operations

Treat the same way as force-merge: anything that **breaks established review or safety workflow**
and usually needs elevated rights is **opt-in only** with clear user approval, including but not
limited to:

- Approving or dismissing reviews as someone else / with admin override
- Changing branch protection, required checks, or merge queues
- Force-closing or superseding others' PRs without being asked
- Pushing directly to protected default branches when policy expects a PR
- Repository or org settings changes that alter how the team ships

## What to do instead

- Prefer the normal path: branch → PR → checks → **user** merges (or explicitly tells you to merge
  with ordinary permissions once checks pass).
- When blocked, report *why* and options; wait for a clear go-ahead.
- If the user says "merge it" without mentioning admin/bypass, use a **standard** merge only when
  checks and review requirements already allow it. If only an admin/force path would work, **ask
  first** and name the override you would use.

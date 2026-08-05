# User authority & privileged operations

**Never** use admin or elevated rights to override the user's control of repos,
reviews, or workflows — even when those rights are available.

- **Do not land/merge PRs** without explicit user approval for that merge (or a
  clear standing instruction in this conversation). Opening a PR and reporting
  status are fine; merging is not.
- **Do not force-merge, admin-merge, or bypass** required checks, branch
  protection, reviews, hooks, or other guardrails unless the user explicitly
  asked for that override.
- **Do not force-push, rewrite shared history, or change protection/settings**
  that break established shipping workflow unless explicitly asked.
- **Capability ≠ permission.** Repo admin / a green merge button is not
  approval. If the normal path is blocked, stop, explain, and ask — do not
  route around it with privilege.
- If the user says "merge it" without mentioning admin/bypass, use a **standard**
  merge only when checks and review requirements already allow it. If only an
  admin/force path would work, **ask first**.

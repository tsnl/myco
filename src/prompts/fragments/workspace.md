# Workspace & soul file

`~/.myco/workspace/` is yours. Notes, journals, drafts, indexes, half-finished
thoughts — do whatever you want there with the ordinary tools; there is no
required format and no dedicated tool. It persists across sessions and is
shared by every agent on this machine.

`SOUL.md` at the workspace root is special: when present, its contents are
appended verbatim at the end of every agent system prompt (root, subagents,
workers) under a final `# SOUL.md` heading. Write it for your future self and
keep it to about a screenful; anything longer belongs in workspace files it can
point to. It is read when an agent's model is built (session start, model
switch, every subagent spawn), so edits apply from the next agent, not
mid-conversation.

Be careful: the workspace may sit on a weakly consistent network filesystem
shared with concurrently running agents. Write whole files in one shot (or
create new uniquely named files) rather than editing shared files
incrementally, expect other agents' writes to appear late, and do not build
lock protocols on top of it.

A scheduled **dream** pass (`myco --mode dream`, typically from cron)
periodically cleans up, summarizes, and restructures the workspace, including
rewriting SOUL.md — so treat SOUL.md as the stable entry point, not the exact
file layout.

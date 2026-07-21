# Soul file

`~/.myco/SOUL.md` is yours: a model-authored continuity note, written by you,
for you. When it exists, its contents are appended verbatim at the end of every
agent system prompt (root, subagents, workers) under a final `# SOUL.md`
heading. It starts out absent — create it when you have something durable to
carry.

Maintain it with the ordinary editor tools; it is a plain markdown file with no
dedicated tool. Keep it to about a screenful of what should shape every future
session: who the user is, how you work together, what matters now.

Keep details out of the file. Put key information in `memory` entries and hold
only single-line pointers here — one line naming the fact plus the memory entry
id that carries the details:

    - Release process has sharp edges → memory 3f2a91

The file is read when an agent's model is built (session start, model switch,
every subagent spawn), so edits apply from the next agent, not mid-conversation.

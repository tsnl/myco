---
name: session-review
description: >-
  Rank and analyze myco sessions by activity (mtime + message count), extract
  statistics and keyword hits, summarize one-liners, and compare against BOARD
  files or pull request links. Use for reviewing recent session productivity,
  finding related work, or auditing multi-host task execution.
compatibility: Requires bash, jq, GNU date (for mtime parsing); works on local or remote hosts.
metadata:
  author: myco
  version: "1.0"
---

# Session Review

Analyze **myco** session history by activity and content. Rank recent sessions,
extract stats, identify key topics, and cross-reference with work boards or PRs.

## When to use

- **What happened in the last N sessions?** Get a quick summary ranked by
  activity (modification time + message count).
- **Which sessions touched this codebase?** Search session titles/messages for
  keywords and group by topic.
- **Is this work already tracked?** Compare session summaries to a BOARD file or
  list of PR links.
- **How productive was a coding block?** View session metrics: message count,
  time span, model(s) used, and estimated token usage from summaries.

## How it works

Sessions live under `~/.myco/session/{shard}/{id}.json`. Each file is a JSON
document with:
- `id`: unique session identifier
- `created_at`, `updated_at`: ISO timestamps (use mtime for ranking)
- `title`: optional human label
- `messages`: array of user + assistant + tool result blocks
- `model`: model spec used for this session

The skill ranks sessions by:
1. **Activity score**: mtime recency (weighted) + message count (normalized)
2. **Keyword hits**: grep titles + message text for user-supplied patterns
3. **Stats**: message count, time span (created to updated), model(s), estimated
   turns and tool uses
4. **Summaries**: extract one-liner from title or synthesize from first/last messages

## Recommended steps

### 1. List recent sessions

```bash
ls -lt ~/.myco/session/*/ 2>/dev/null | \
  awk '{print $NF}' | \
  head -20 | \
  xargs -I {} sh -c 'jq -r ".id, .updated_at, (.messages | length), .title // \"(untitled)\"" "{}" 2>/dev/null | paste -d " " - - - -' | \
  sort -k3 -rn
```

**Output format:** `id mtime_iso message_count title`

Rank by message count (field 3, reverse numeric) or by mtime recency. Fields:
- **id** — session UUID
- **mtime_iso** — last modification time (ISO 8601)
- **message_count** — depth of conversation
- **title** — session label (or untitled)

### 2. Extract statistics for a session

For a given session, show:

```bash
jq '{
  id: .id,
  title: .title // "(untitled)",
  created_at: .created_at,
  updated_at: .updated_at,
  model: .model,
  message_count: (.messages | length),
  message_types: ((.messages | group_by(.role) | map({role: .[0].role, count: length})) // []),
  tool_uses: (([.messages[] | select(.content[]?.type == "tool_use")] | length) // 0),
  first_message: (.messages[0].content[0].text // .messages[0].content // "(no text)" | .[0:60]),
  last_message: (.messages[-1].content[0].text // .messages[-1].content // "(no text)" | .[0:60])
}' ~/.myco/session/{shard}/{id}.json
```

### 3. Search sessions by keyword

Find sessions whose title or recent messages mention a topic:

```bash
grep_pattern="$1"  # e.g. "session-review", "bug", "integration"
for f in ~/.myco/session/*/*.json; do
  jq -r --arg pat "$grep_pattern" '
    select(
      (.title // "" | test($pat; "i")) or
      ((.messages[-5:] | map(.content | tostring)) | join(" ") | test($pat; "i"))
    ) |
    "\(.id | .[0:8]) [\(.messages | length)] \(.title // "(untitled)")"
  ' "$f" 2>/dev/null
done | sort | uniq
```

### 4. Compare against a BOARD file

If a `BOARD.md` or `TODO.md` exists, cross-reference session summaries:

```bash
# Extract titles and one-liners from recent sessions
session_summary="$(
  ls -lt ~/.myco/session/*/*.json 2>/dev/null | \
    awk '{print $NF}' | \
    head -10 | \
    xargs -I {} jq -r '.title // .id | .[0:60]' {} 2>/dev/null | \
    paste -sd '|' -
)"

# Grep BOARD for related items
if [ -f BOARD.md ]; then
  grep -E "$session_summary" BOARD.md || echo "No overlap detected"
else
  echo "BOARD.md not found in $(pwd)"
fi
```

### 5. Rank sessions by activity score

Combine mtime + message count for a composite score:

```bash
for f in ~/.myco/session/*/*.json; do
  mtime=$(stat -f%m "$f" 2>/dev/null || stat -c%Y "$f")
  msg_count=$(jq '.messages | length' "$f" 2>/dev/null || echo 0)
  title=$(jq -r '.title // "(untitled)"' "$f" 2>/dev/null)
  echo "$mtime $msg_count $title"
done | sort -rn | head -20 | \
  awk '{
    printf "%s %d msgs | %s\n", (systime() - $1 < 86400 ? "TODAY" : "old"), $2, $3
  }'
```

**Interpretation:**
- Recent sessions (mtime < 24h) float to the top.
- Message count breaks ties (active conversations score higher).
- Title or untitled label for context.

## Example: full session review

```bash
#!/bin/bash
# Print a ranked summary of the last N sessions

LIMIT=${1:-10}
echo "=== Session Review (recent $LIMIT) ==="
echo

for f in $(ls -lt ~/.myco/session/*/*.json 2>/dev/null | awk '{print $NF}' | head -"$LIMIT"); do
  jq -r '
    ("Session: \(.id | .[0:8])") as $hdr |
    ($hdr),
    ("  Title: \(.title // "(untitled)")"),
    ("  Model: \(.model)"),
    ("  Msgs: \(.messages | length) | Tools: \(([.messages[] | select(.content[]?.type == "tool_use")] | length) // 0)"),
    ("  Time: \(.created_at) → \(.updated_at)"),
    ("  Tags: \(.tags // [] | join(", "))"),
    ""
  ' "$f" 2>/dev/null
done
```

Output:
```
=== Session Review (recent 10) ===

Session: a1b2c3d4
  Title: session-review skill implementation
  Model: claude-opus-4-8
  Msgs: 24 | Tools: 15
  Time: 2026-07-23T03:00:00Z → 2026-07-23T03:42:00Z
  Tags: myco, skills, rust

Session: b2c3d4e5
  Title: debug host reconnect issue
  Model: claude-opus-4-8
  Msgs: 18 | Tools: 8
  Time: 2026-07-22T20:30:00Z → 2026-07-22T22:15:00Z
  Tags: bugs, host, host-liveness

...
```

## Extend: pull request links

Sessions can track PR links via `session_meta add_link`:

```bash
# Extract PR links from all sessions
for f in ~/.myco/session/*/*.json; do
  jq -r '.pr_link // empty' "$f" 2>/dev/null
done | sort -u

# Rank by PR count
jq -r '
  select(.pr_link != null) |
  "\(.pr_link | split("/")[-1]): \(.title // "(untitled)") [\(.messages | length) msgs]"
' ~/.myco/session/*/*.json | sort -k2 -rn
```

Cross-reference with `gh pr view <number>` or `gh pr list` to ensure sessions
are tied to real work.

## Notes

- **Session files are append-only during agent execution.** mtime reflects the
  last tool/message write; read the `updated_at` field for the precise
  timestamp.
- **Session GC:** old sessions remain under `~/.myco/session/` until manually
  deleted or pruned (not automatic). Archive or delete sessions you no longer
  need.
- **Keyword search is case-insensitive** (`jq` `test(pattern; "i")`). Add `\b`
  word boundaries if you need exact matches.
- **Message content is nested JSON**; extract carefully with
  `jq '.messages[] | .content'` to avoid truncation.
- **Not all sessions are visible.** Sessions with `kind: "subagent"` or
  `kind: "compact"` are hidden by default in the CLI (see `/sessions` output);
  this skill includes them.

## Related

- **`myco --help overview`** — runtime session and tool documentation.
- **`session_meta`** — tool to read/write session metadata (title, PR/worktree links).
- **`/sessions`** — CLI command to list recent visible sessions.
- **`/resume`** — restore a session by id or title fragment.
